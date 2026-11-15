//! Stage A: a SOCKS5 server (RFC 1928) whose CONNECT and UDP ASSOCIATE
//! open flows on the phone. No auth, no BIND. Names are passed through to
//! the phone, which resolves them on the cellular connection.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use lightning_router_frame::{
    FrameReader, FrameWriter, MAX_DATAGRAM,
    flow::{Addr, Request, Status},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    task::JoinSet,
    time::Instant,
};

use crate::{device::Cache, flow};

pub const DEFAULT_ADDR: &str = "127.0.0.1:1080";
/// A UDP flow with no traffic in either direction for this long is closed.
pub const UDP_IDLE: Duration = Duration::from_secs(60);
/// How long a client has to finish the SOCKS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

const VERSION: u8 = 5;
const CMD_CONNECT: u8 = 1;
const CMD_UDP: u8 = 3;

const REP_OK: u8 = 0x00;
const REP_FAILURE: u8 = 0x01;
const REP_UNREACHABLE: u8 = 0x03;
const REP_HOST_UNREACHABLE_NAME: u8 = 0x04;
const REP_REFUSED: u8 = 0x05;
const REP_CMD_UNSUPPORTED: u8 = 0x07;
const REP_ATYP_UNSUPPORTED: u8 = 0x08;

/// The SOCKS5 reply code for a phone status (the spec's table).
pub fn reply_for(status: Status) -> u8 {
    match status {
        Status::Ok => REP_OK,
        Status::Failed | Status::Busy => REP_FAILURE,
        Status::Refused => REP_REFUSED,
        Status::Unreachable => REP_UNREACHABLE,
        Status::NoSuchName | Status::TimedOut => REP_HOST_UNREACHABLE_NAME,
        Status::Unsupported => REP_CMD_UNSUPPORTED,
    }
}

pub async fn run(addr: &str, cache: Cache) {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("Failed to bind SOCKS5 address {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "SOCKS5 proxy on {}",
        listener.local_addr().map(|a| a.to_string()).unwrap_or_default()
    );
    serve(listener, cache, UDP_IDLE).await;
}

/// Accepts clients forever. Dropping the future aborts every connection.
pub async fn serve(listener: TcpListener, cache: Cache, udp_idle: Duration) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let cache = cache.clone();
                    clients.spawn(async move {
                        let _ = client(stream, cache, udp_idle).await;
                    });
                }
                Err(e) => {
                    eprintln!("SOCKS accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

enum Command {
    Connect(Addr, u16),
    Udp,
}

async fn client(mut stream: TcpStream, cache: Cache, udp_idle: Duration) -> io::Result<()> {
    let command = match tokio::time::timeout(HANDSHAKE, handshake(&mut stream)).await {
        Ok(Ok(Some(command))) => command,
        _ => return Ok(()),
    };
    match command {
        Command::Connect(addr, port) => {
            match flow::open_flow(&cache, &Request::tcp(addr, port)).await {
                Ok(mut flow) => {
                    reply(&mut stream, REP_OK, unspecified()).await?;
                    let _ = flow::splice(&mut flow, &mut stream).await;
                }
                Err(status) => reply(&mut stream, reply_for(status), unspecified()).await?,
            }
            Ok(())
        }
        Command::Udp => associate(stream, cache, udp_idle).await,
    }
}

fn unspecified() -> SocketAddr {
    (Ipv4Addr::UNSPECIFIED, 0).into()
}

/// Negotiates the method and reads the request. `None` means the client was
/// refused (the reply is already sent).
async fn handshake(stream: &mut TcpStream) -> io::Result<Option<Command>> {
    let mut head = [0; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Ok(None);
    }
    let mut methods = vec![0; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[VERSION, 0xFF]).await?;
        return Ok(None);
    }
    stream.write_all(&[VERSION, 0]).await?;

    let mut req = [0; 3];
    stream.read_exact(&mut req).await?;
    if req[0] != VERSION {
        return Ok(None);
    }
    let target = read_addr(stream).await?;
    let cmd = req[1];
    match (cmd, target) {
        (_, Err(rep)) => {
            reply(stream, rep, unspecified()).await?;
            Ok(None)
        }
        (CMD_CONNECT, Ok((addr, port))) => Ok(Some(Command::Connect(addr, port))),
        (CMD_UDP, Ok(_)) => Ok(Some(Command::Udp)),
        _ => {
            reply(stream, REP_CMD_UNSUPPORTED, unspecified()).await?;
            Ok(None)
        }
    }
}

/// Reads ATYP, address and port. The inner error is the SOCKS reply code
/// for an address the phone can't be asked about.
async fn read_addr(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> io::Result<Result<(Addr, u16), u8>> {
    let atyp = stream.read_u8().await?;
    let addr = match atyp {
        1 => {
            let mut b = [0; 4];
            stream.read_exact(&mut b).await?;
            Addr::V4(b.into())
        }
        4 => {
            let mut b = [0; 16];
            stream.read_exact(&mut b).await?;
            Addr::V6(b.into())
        }
        3 => {
            let len = stream.read_u8().await? as usize;
            let mut b = vec![0; len];
            stream.read_exact(&mut b).await?;
            // The flow protocol carries ASCII names of 1 to 255 bytes.
            match String::from_utf8(b) {
                Ok(name) if !name.is_empty() && name.is_ascii() => Addr::Domain(name),
                _ => {
                    let _ = stream.read_u16().await;
                    return Ok(Err(REP_ATYP_UNSUPPORTED));
                }
            }
        }
        _ => return Ok(Err(REP_ATYP_UNSUPPORTED)),
    };
    let port = stream.read_u16().await?;
    Ok(Ok((addr, port)))
}

async fn reply(stream: &mut TcpStream, code: u8, bound: SocketAddr) -> io::Result<()> {
    let mut out = vec![VERSION, code, 0];
    write_sock_addr(&mut out, bound);
    stream.write_all(&out).await
}

fn write_sock_addr(out: &mut Vec<u8>, addr: SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

// ---- UDP ASSOCIATE ----

type Dest = (Addr, u16);

/// Parses a SOCKS UDP datagram into its destination and payload. `None`
/// drops it (fragmented, malformed or an unsupported address).
pub fn parse_udp(datagram: &[u8]) -> Option<(Dest, &[u8])> {
    if datagram.len() < 4 || datagram[2] != 0 {
        return None;
    }
    let rest = &datagram[4..];
    let (addr, rest) = match datagram[3] {
        1 => {
            let b: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            (Addr::V4(b.into()), &rest[4..])
        }
        4 => {
            let b: [u8; 16] = rest.get(..16)?.try_into().ok()?;
            (Addr::V6(b.into()), &rest[16..])
        }
        3 => {
            let len = *rest.first()? as usize;
            let name = rest.get(1..1 + len)?;
            let name = std::str::from_utf8(name).ok()?;
            if name.is_empty() || !name.is_ascii() {
                return None;
            }
            (Addr::Domain(name.to_owned()), &rest[1 + len..])
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
    Some(((addr, port), &rest[2..]))
}

/// Builds the SOCKS UDP header for a reply from `dest`, then the payload.
pub fn build_udp(dest: &Dest, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0];
    match &dest.0 {
        Addr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        Addr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        Addr::Domain(name) => {
            out.push(3);
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
    }
    out.extend_from_slice(&dest.1.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// One phone UDP flow, run as a task: datagrams from `rx` go out as frames
/// and frames come back on `replies`, until idle, a failure, or the flow
/// ends. Frame reads and writes each get their own loop (not cancel-safe).
async fn udp_flow(
    cache: Cache,
    dest: Dest,
    mut rx: mpsc::Receiver<Vec<u8>>,
    replies: mpsc::Sender<(Dest, Vec<u8>)>,
    idle: Duration,
) {
    let request = Request::udp(dest.0.clone(), dest.1);
    let Ok(stream) = flow::open_flow(&cache, &request).await else {
        return;
    };
    let (read, write) = tokio::io::split(stream);
    let mut frames_in = FrameReader::new(read);
    let mut frames_out = FrameWriter::new(write);
    let last = std::sync::Mutex::new(Instant::now());
    let uplink = async {
        while let Some(datagram) = rx.recv().await {
            *last.lock().unwrap() = Instant::now();
            if frames_out.send(&datagram).await.is_err() {
                return;
            }
        }
    };
    let downlink = async {
        while let Ok(Some(datagram)) = frames_in.next().await {
            *last.lock().unwrap() = Instant::now();
            if replies.send((dest.clone(), datagram.to_vec())).await.is_err() {
                return;
            }
        }
    };
    let watchdog = async {
        loop {
            let deadline = *last.lock().unwrap() + idle;
            tokio::time::sleep_until(deadline).await;
            if *last.lock().unwrap() + idle <= Instant::now() {
                return;
            }
        }
    };
    tokio::select! {
        () = uplink => {}
        () = downlink => {}
        () = watchdog => {}
    }
}

/// Serves one UDP ASSOCIATE until its control connection closes.
async fn associate(mut control: TcpStream, cache: Cache, idle: Duration) -> io::Result<()> {
    let ip = control.local_addr()?.ip();
    let socket = match UdpSocket::bind((ip, 0)).await {
        Ok(socket) => socket,
        Err(_) => return reply(&mut control, REP_FAILURE, unspecified()).await,
    };
    reply(&mut control, REP_OK, socket.local_addr()?).await?;

    // Dropping the set on return aborts every flow task and so closes them.
    let mut tasks = JoinSet::new();
    let mut flows: HashMap<Dest, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let (reply_tx, mut reply_rx) = mpsc::channel::<(Dest, Vec<u8>)>(64);
    let mut client: Option<SocketAddr> = None;
    let mut buf = vec![0; MAX_DATAGRAM + 300];
    let mut ignore = [0; 64];

    loop {
        tokio::select! {
            // Any byte or EOF on the control connection ends the association.
            _ = control.read(&mut ignore) => return Ok(()),
            received = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = received else { return Ok(()) };
                // The first sender owns the association.
                if *client.get_or_insert(from) != from {
                    continue;
                }
                let Some((dest, payload)) = parse_udp(&buf[..n]) else { continue };
                let payload = payload.to_vec();
                let live = flows.get(&dest).is_some_and(|tx| !tx.is_closed());
                if !live {
                    let (tx, rx) = mpsc::channel(64);
                    tasks.spawn(udp_flow(cache.clone(), dest.clone(), rx, reply_tx.clone(), idle));
                    flows.insert(dest.clone(), tx);
                }
                // A full queue drops the datagram, as UDP may.
                let _ = flows[&dest].try_send(payload);
            }
            Some((dest, payload)) = reply_rx.recv() => {
                if let Some(client) = client {
                    let _ = socket.send_to(&build_udp(&dest, &payload), client).await;
                }
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, State};
    use std::net::Ipv6Addr;
    use lightning_router::flow::{FlowLimits, serve_flows};

    const PATIENCE: Duration = Duration::from_secs(5);

    fn limits() -> FlowLimits {
        FlowLimits {
            max_flows: 64,
            header_deadline: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(5),
        }
    }

    fn phone() -> (lightning_router::ProxyHandle, Cache) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = serve_flows(vec![listener], limits()).unwrap();
        let cache = Cache::new();
        cache.set(State::Ready(Device::Addr(addr)));
        (handle, cache)
    }

    /// Starts the SOCKS server on loopback and returns its address.
    async fn socks(cache: Cache, idle: Duration) -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, cache, idle));
        addr
    }

    async fn stop(handle: lightning_router::ProxyHandle) {
        tokio::task::spawn_blocking(move || handle.stop()).await.unwrap();
    }

    fn atyp_bytes(addr: &Addr, port: u16) -> Vec<u8> {
        let mut out = Vec::new();
        match addr {
            Addr::V4(ip) => {
                out.push(1);
                out.extend_from_slice(&ip.octets());
            }
            Addr::V6(ip) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            }
            Addr::Domain(n) => {
                out.push(3);
                out.push(n.len() as u8);
                out.extend_from_slice(n.as_bytes());
            }
        }
        out.extend_from_slice(&port.to_be_bytes());
        out
    }

    /// Greets, sends a command and returns the stream and the reply code.
    async fn command(proxy: SocketAddr, cmd: u8, addr: &Addr, port: u16) -> (TcpStream, u8, SocketAddr) {
        let mut s = TcpStream::connect(proxy).await.unwrap();
        s.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0; 2];
        s.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);
        let mut req = vec![5, cmd, 0];
        req.extend(atyp_bytes(addr, port));
        s.write_all(&req).await.unwrap();
        let mut head = [0; 4];
        s.read_exact(&mut head).await.unwrap();
        let bound = match head[3] {
            1 => {
                let mut b = [0; 4];
                s.read_exact(&mut b).await.unwrap();
                IpAddr::from(b)
            }
            _ => {
                let mut b = [0; 16];
                s.read_exact(&mut b).await.unwrap();
                IpAddr::from(b)
            }
        };
        let port = s.read_u16().await.unwrap();
        (s, head[1], SocketAddr::new(bound, port))
    }

    async fn echo_tcp(listener: TcpListener) {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = [0; 5];
        s.read_exact(&mut buf).await.unwrap();
        s.write_all(&buf).await.unwrap();
    }

    async fn round_trip(proxy: SocketAddr, addr: Addr, port: u16) {
        let (mut s, rep, _) = command(proxy, CMD_CONNECT, &addr, port).await;
        assert_eq!(rep, REP_OK);
        s.write_all(b"hello").await.unwrap();
        let mut back = [0; 5];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socks_connect_by_domain_reaches_target() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(echo_tcp(target));
        tokio::time::timeout(PATIENCE, round_trip(proxy, Addr::Domain("localhost".into()), port))
            .await
            .expect("stalled");
        stop(handle).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socks_connect_by_ipv4_and_ipv6_reaches_target() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let v4_port = v4.local_addr().unwrap().port();
        tokio::spawn(echo_tcp(v4));
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let v6_port = v6.local_addr().unwrap().port();
        tokio::spawn(echo_tcp(v6));
        let work = async {
            round_trip(proxy, Addr::V4(Ipv4Addr::LOCALHOST), v4_port).await;
            round_trip(proxy, Addr::V6(Ipv6Addr::LOCALHOST), v6_port).await;
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        stop(handle).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phone_status_maps_to_socks_reply() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let closed = {
            let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            l.local_addr().unwrap().port()
        };
        let work = async {
            let (_, rep, _) =
                command(proxy, CMD_CONNECT, &Addr::V4(Ipv4Addr::LOCALHOST), closed).await;
            assert_eq!(rep, REP_REFUSED);
            let (_, rep, _) = command(
                proxy,
                CMD_CONNECT,
                &Addr::Domain("no-such-name.invalid".into()),
                80,
            )
            .await;
            assert_eq!(rep, 0x04);
            // BIND is not supported.
            let (_, rep, _) = command(proxy, 2, &Addr::V4(Ipv4Addr::LOCALHOST), 80).await;
            assert_eq!(rep, 0x07);
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        stop(handle).await;
    }

    #[test]
    fn every_status_maps_per_the_spec_table() {
        let expected = [0x00, 0x01, 0x05, 0x03, 0x04, 0x04, 0x07, 0x01];
        for (status, want) in Status::ALL.iter().zip(expected) {
            assert_eq!(reply_for(*status), want, "{status:?}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socks_rejects_bind_and_auth_only_clients() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let work = async {
            // A client offering only username/password (0x02).
            let mut s = TcpStream::connect(proxy).await.unwrap();
            s.write_all(&[5, 1, 2]).await.unwrap();
            let mut method = [0; 2];
            s.read_exact(&mut method).await.unwrap();
            assert_eq!(method, [5, 0xFF]);
            // BIND after a good greeting.
            let (_, rep, _) = command(proxy, 2, &Addr::V4(Ipv4Addr::LOCALHOST), 1).await;
            assert_eq!(rep, 0x07);
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        stop(handle).await;
    }

    async fn echo_udp() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = echo.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = [0; 64];
            loop {
                let (n, from) = echo.recv_from(&mut buf).await.unwrap();
                echo.send_to(&buf[..n], from).await.unwrap();
            }
        });
        (addr, task)
    }

    async fn associate_client(proxy: SocketAddr) -> (TcpStream, UdpSocket) {
        let (control, rep, bound) =
            command(proxy, CMD_UDP, &Addr::V4(Ipv4Addr::UNSPECIFIED), 0).await;
        assert_eq!(rep, REP_OK);
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        udp.connect(bound).await.unwrap();
        (control, udp)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn socks_udp_associate_reaches_two_destinations() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let (a, _ta) = echo_udp().await;
        let (b, _tb) = echo_udp().await;
        let work = async {
            let (_control, udp) = associate_client(proxy).await;
            for (dest, msg) in [(a, &b"to-a"[..]), (b, &b"to-b"[..])] {
                let packet = build_udp(&(Addr::V4(Ipv4Addr::LOCALHOST), dest.port()), msg);
                udp.send(&packet).await.unwrap();
                let mut buf = [0; 256];
                let n = udp.recv(&mut buf).await.unwrap();
                let (from, payload) = parse_udp(&buf[..n]).unwrap();
                assert_eq!(from, (Addr::V4(Ipv4Addr::LOCALHOST), dest.port()));
                assert_eq!(payload, msg);
            }
        };
        tokio::time::timeout(PATIENCE, work).await.expect("stalled");
        stop(handle).await;
    }

    async fn wait_for_flows(handle: &lightning_router::ProxyHandle, n: u64) {
        let work = async {
            while handle.stats().connections != n {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(PATIENCE, work)
            .await
            .unwrap_or_else(|_| panic!("flow count never reached {n}"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn udp_association_ends_with_its_control_connection() {
        let (handle, cache) = phone();
        let proxy = socks(cache, UDP_IDLE).await;
        let (a, _ta) = echo_udp().await;
        let (control, udp) = associate_client(proxy).await;
        let packet = build_udp(&(Addr::V4(Ipv4Addr::LOCALHOST), a.port()), b"x");
        udp.send(&packet).await.unwrap();
        let mut buf = [0; 256];
        tokio::time::timeout(PATIENCE, udp.recv(&mut buf)).await.unwrap().unwrap();
        wait_for_flows(&handle, 1).await;
        drop(control);
        wait_for_flows(&handle, 0).await;
        stop(handle).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_udp_flow_closes_after_timeout() {
        let (handle, cache) = phone();
        // A short real timeout: paused time would also skip the socket waits.
        let proxy = socks(cache, Duration::from_millis(300)).await;
        let (a, _ta) = echo_udp().await;
        let (_control, udp) = associate_client(proxy).await;
        let packet = build_udp(&(Addr::V4(Ipv4Addr::LOCALHOST), a.port()), b"x");
        udp.send(&packet).await.unwrap();
        let mut buf = [0; 256];
        tokio::time::timeout(PATIENCE, udp.recv(&mut buf)).await.unwrap().unwrap();
        wait_for_flows(&handle, 1).await;
        wait_for_flows(&handle, 0).await;
        stop(handle).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_device_fails_flows_fast() {
        let proxy = socks(Cache::new(), UDP_IDLE).await;
        let started = std::time::Instant::now();
        let (_, rep, _) =
            command(proxy, CMD_CONNECT, &Addr::V4(Ipv4Addr::LOCALHOST), 80).await;
        assert_eq!(rep, REP_FAILURE);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn socks_listens_on_loopback_by_default() {
        let addr: SocketAddr = DEFAULT_ADDR.parse().unwrap();
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn udp_header_round_trips_and_drops_fragments() {
        let dest = (Addr::Domain("example.com".into()), 53);
        let packet = build_udp(&dest, b"data");
        assert_eq!(parse_udp(&packet), Some((dest, &b"data"[..])));
        let mut fragmented = packet.clone();
        fragmented[2] = 1;
        assert_eq!(parse_udp(&fragmented), None);
        assert_eq!(parse_udp(&packet[..5]), None);
    }
}
