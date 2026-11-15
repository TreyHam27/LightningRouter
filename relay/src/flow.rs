//! Opening flows on the phone (D1) and moving bytes and datagrams through
//! them. The SOCKS and system modes build on this.
// The callers arrive in later steps; tests exercise everything here.
#![allow(dead_code)]

use std::io;

use idevice::ReadWrite;
use lightning_router_frame::{
    FLOW_PORT, FrameReader, FrameWriter, MAX_DATAGRAM,
    flow::{Request, Status, read_status, write_request},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, copy_bidirectional},
    net::UdpSocket,
};

use crate::device::{self, Cache};

/// An open flow: after `Status::Ok`, raw bytes (TCP) or frames (UDP).
pub type Stream = Box<dyn ReadWrite>;

/// Opens one flow on a fresh connection to the phone's flow port. A failed
/// connect marks the cache stale and fails at once; there is no retry here.
pub async fn open_flow(cache: &Cache, request: &Request) -> Result<Stream, Status> {
    let device = cache.device().map_err(|_| Status::Failed)?;
    let mut stream = match device::connect(&device, FLOW_PORT).await {
        Ok(stream) => stream,
        Err(reason) => {
            cache.mark_stale(reason);
            return Err(Status::Failed);
        }
    };
    write_request(&mut stream, request)
        .await
        .map_err(|_| Status::Failed)?;
    match read_status(&mut stream).await {
        Ok(Status::Ok) => Ok(stream),
        Ok(status) => Err(status),
        Err(_) => Err(Status::Failed),
    }
}

/// Splices a TCP flow with a local stream until both directions are done.
pub async fn splice(
    flow: &mut (impl AsyncRead + AsyncWrite + Unpin),
    local: &mut (impl AsyncRead + AsyncWrite + Unpin),
) -> io::Result<(u64, u64)> {
    copy_bidirectional(local, flow).await
}

/// Pumps a UDP flow: datagrams from `socket` (connected to the local peer)
/// go out as frames, and frames come back as datagrams, until the flow ends
/// or a socket fails. Each direction has its own loop, because frame reads
/// and writes are not cancel-safe.
pub async fn pump_udp(flow: impl AsyncRead + AsyncWrite, socket: &UdpSocket) -> io::Result<()> {
    let (flow_in, flow_out) = tokio::io::split(flow);
    let mut frames_out = FrameWriter::new(flow_out);
    let mut frames_in = FrameReader::new(flow_in);
    let uplink = async {
        let mut datagram = vec![0; MAX_DATAGRAM];
        loop {
            let len = socket.recv(&mut datagram).await?;
            frames_out.send(&datagram[..len]).await?;
        }
    };
    let downlink = async {
        while let Some(datagram) = frames_in.next().await? {
            socket.send(datagram).await?;
        }
        Ok(())
    };
    tokio::select! {
        done = uplink => done,
        done = downlink => done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, State};
    use lightning_router::flow::{FlowLimits, serve_flows};
    use lightning_router_frame::flow::Addr;
    use std::{net::Ipv4Addr, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const PATIENCE: Duration = Duration::from_secs(5);

    fn limits() -> FlowLimits {
        FlowLimits {
            max_flows: 64,
            header_deadline: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// Starts the real phone flow server on loopback; the cache points at it.
    fn phone() -> (lightning_router::ProxyHandle, Cache) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = serve_flows(vec![listener], limits()).unwrap();
        let cache = Cache::new();
        cache.set(State::Ready(Device::Addr(addr)));
        (handle, cache)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tcp_flow_reaches_a_target_through_the_real_flow_server() {
        let (handle, cache) = phone();
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = target.accept().await.unwrap();
            let mut buf = [0; 5];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });

        let work = async {
            let mut flow = open_flow(&cache, &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), port))
                .await
                .unwrap();
            flow.write_all(b"hello").await.unwrap();
            let mut back = [0; 5];
            flow.read_exact(&mut back).await.unwrap();
            assert_eq!(&back, b"hello");
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        drop(cache);
        tokio::task::spawn_blocking(move || handle.stop())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refused_target_reports_its_status_and_stale_device_fails_fast() {
        let (handle, cache) = phone();
        // A port nobody listens on.
        let closed = {
            let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let refused = open_flow(&cache, &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), closed)).await;
        assert!(matches!(refused, Err(Status::Refused)));

        // No device: fails at once without touching the network.
        let none = Cache::new();
        let started = std::time::Instant::now();
        let failed = open_flow(&none, &Request::ping()).await;
        assert!(matches!(failed, Err(Status::Failed)));
        assert!(started.elapsed() < Duration::from_secs(1));

        // An unreachable device marks the cache stale.
        let dead = Cache::new();
        dead.set(State::Ready(Device::Addr(([127, 0, 0, 1], 1).into())));
        assert!(open_flow(&dead, &Request::ping()).await.is_err());
        assert!(dead.device().is_err());
        tokio::task::spawn_blocking(move || handle.stop())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_flow_round_trips_through_the_pump() {
        let (handle, cache) = phone();
        let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0; 64];
            let (n, from) = echo.recv_from(&mut buf).await.unwrap();
            echo.send_to(&buf[..n], from).await.unwrap();
        });

        let work = async {
            let flow = open_flow(&cache, &Request::udp(Addr::V4(Ipv4Addr::LOCALHOST), port))
                .await
                .unwrap();
            // `local` stands in for the Mac-side socket the pump serves.
            let local = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let app = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            local.connect(app.local_addr().unwrap()).await.unwrap();
            app.connect(local.local_addr().unwrap()).await.unwrap();
            let exchange = async {
                app.send(b"ping").await.unwrap();
                let mut buf = [0; 16];
                let n = app.recv(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], b"ping");
            };
            tokio::select! {
                ended = pump_udp(flow, &local) => panic!("pump ended early: {ended:?}"),
                () = exchange => {}
            }
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        tokio::task::spawn_blocking(move || handle.stop())
            .await
            .unwrap();
    }
}
