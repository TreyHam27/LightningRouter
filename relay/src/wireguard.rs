// Jackson Coxson

//! WireGuard mode: takes WireGuard's UDP datagrams and exchanges them,
//! framed, with the LightningRouter app over usbmuxd.

use std::{
    cell::Cell,
    io,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use lightning_router_frame::{DEVICE_PORT, FrameReader, FrameWriter, MAX_DATAGRAM, RELAY_PORT};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
};

use crate::device;

/// How long to wait before looking for the device or the app again.
const RETRY: Duration = Duration::from_secs(1);
/// How long to wait before reconnecting to an app that was just connected.
const RECONNECT: Duration = Duration::from_millis(100);

/// The WireGuard client replies go to: whoever sent the last datagram.
type Client = Cell<Option<SocketAddr>>;

pub async fn run() {
    // Bound once and kept across reconnects, so WireGuard keeps talking to
    // the same socket while the app comes and goes.
    let clients = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, RELAY_PORT)).await {
        Ok(clients) => clients,
        Err(e) => {
            eprintln!("Failed to bind UDP port {RELAY_PORT}: {e}");
            std::process::exit(1);
        }
    };
    println!("Listening for WireGuard on UDP port {RELAY_PORT}");

    let client = Client::default();
    let mut waiting_for = None;
    loop {
        match device::connect_usbmuxd(DEVICE_PORT).await {
            Ok(link) => {
                waiting_for = None;
                match bridge(link, &clients, &client).await {
                    Ok(()) => println!("The app closed the connection, reconnecting"),
                    Err(e) => println!("Lost the app ({e}), reconnecting"),
                }
                tokio::time::sleep(RECONNECT).await;
            }
            Err(reason) => {
                // Said once per reason, not once per attempt.
                if waiting_for.as_ref() != Some(&reason) {
                    println!("Waiting: {reason}");
                    waiting_for = Some(reason);
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Relays between the WireGuard client and the app until the link fails or
/// the app hangs up.
async fn bridge(
    link: impl AsyncRead + AsyncWrite,
    clients: &UdpSocket,
    client: &Client,
) -> io::Result<()> {
    let (frames_in, frames_out) = tokio::io::split(link);

    // Each direction runs in its own loop, so neither waits on the other and
    // neither is cancelled mid-frame while the link is healthy.
    tokio::select! {
        done = uplink(clients, client, FrameWriter::new(frames_out)) => done,
        done = downlink(FrameReader::new(frames_in), clients, client) => done,
    }
}

async fn uplink(
    clients: &UdpSocket,
    client: &Client,
    mut frames: FrameWriter<impl AsyncWrite + Unpin>,
) -> io::Result<()> {
    let mut datagram = vec![0; MAX_DATAGRAM];
    loop {
        let (len, from) = clients.recv_from(&mut datagram).await?;
        client.set(Some(from));
        frames.send(&datagram[..len]).await?;
    }
}

async fn downlink(
    mut frames: FrameReader<impl AsyncRead + Unpin>,
    clients: &UdpSocket,
    client: &Client,
) -> io::Result<()> {
    while let Some(datagram) = frames.next().await? {
        // Until a client has spoken there is nobody to deliver to.
        if let Some(client) = client.get() {
            clients.send_to(datagram, client).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::duplex, time::timeout};

    const PATIENCE: Duration = Duration::from_secs(5);

    async fn local_socket() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap()
    }

    #[tokio::test]
    async fn carries_datagrams_between_the_client_and_the_app() {
        let clients = local_socket().await;
        let relay = clients.local_addr().unwrap();
        let client = Client::default();
        let (link, app) = duplex(MAX_DATAGRAM);
        let (app_in, app_out) = tokio::io::split(app);
        let (mut app_in, mut app_out) = (FrameReader::new(app_in), FrameWriter::new(app_out));
        let wireguard = local_socket().await;

        let exchange = async {
            wireguard.send_to(b"handshake", relay).await.unwrap();
            assert_eq!(app_in.next().await.unwrap(), Some(&b"handshake"[..]));

            app_out.send(b"response").await.unwrap();
            let mut datagram = [0; 64];
            let (len, from) = wireguard.recv_from(&mut datagram).await.unwrap();
            assert_eq!((&datagram[..len], from), (&b"response"[..], relay));
        };

        tokio::select! {
            ended = bridge(link, &clients, &client) => panic!("bridge ended early: {ended:?}"),
            done = timeout(PATIENCE, exchange) => done.expect("exchange stalled"),
        }
    }

    #[tokio::test]
    async fn drops_datagrams_nobody_is_waiting_for_and_ends_when_the_app_hangs_up() {
        let clients = local_socket().await;
        let (link, mut app) = duplex(64);
        FrameWriter::new(&mut app).send(b"too early").await.unwrap();
        drop(app);

        let ended = timeout(PATIENCE, bridge(link, &clients, &Client::default())).await;
        assert!(matches!(ended, Ok(Ok(()))), "got {ended:?}");
    }
}
