//! Serverless mode's flow server: each accepted connection carries one flow.
//! The relay sends a request ("TCP to host:port", "UDP to host:port" or a
//! ping), the phone opens the socket, answers with one status byte, then
//! carries bytes. The wire format is `lightning_router_frame::flow`.
//!
//! The server listens on loopback only, so a Wi-Fi neighbour cannot use the
//! phone as an open proxy; usbmuxd delivers USB connections from loopback.

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use lightning_router_frame::{
    FrameReader, FrameWriter,
    flow::{Addr, Kind, ReadError, Request, Status, read_request, write_status},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UdpSocket, lookup_host},
    sync::Semaphore,
    time::timeout,
};

use crate::{Connected, Counters, ProxyHandle, Service, downlink, uplink};

/// Largest soft open-file limit the server asks for.
const FD_LIMIT: u64 = 8192;
/// Descriptors kept back for listeners, the runtime and everything else.
const FD_RESERVE: u64 = 64;

/// Tunables, so tests can shrink them.
#[derive(Clone, Copy, Debug)]
pub struct FlowLimits {
    /// Concurrent flows allowed. Over this, a flow gets `Status::Busy`.
    pub max_flows: usize,
    /// How long the request header may take to arrive.
    pub header_deadline: Duration,
    /// How long resolving and connecting a TCP flow may take in all.
    pub connect_timeout: Duration,
}

impl FlowLimits {
    /// Limits for a process whose open-file soft limit is `fd_limit`: each
    /// flow uses two descriptors.
    pub fn for_fd_limit(fd_limit: u64) -> Self {
        Self {
            max_flows: (fd_limit.saturating_sub(FD_RESERVE) / 2).max(1) as usize,
            header_deadline: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// The per-connection logic behind `Service::Flows`.
pub struct FlowServer {
    limits: FlowLimits,
    slots: Arc<Semaphore>,
}

impl FlowServer {
    pub fn new(limits: FlowLimits) -> Self {
        Self {
            limits,
            slots: Arc::new(Semaphore::new(limits.max_flows)),
        }
    }

    /// Serves one connection to its end.
    pub(crate) async fn serve(&self, stream: TcpStream, peer: SocketAddr, counters: &Counters) {
        if !peer.ip().is_loopback() {
            eprintln!("Flow from {peer} refused: not loopback");
            return;
        }
        if let Err(e) = self.flow(stream, counters).await {
            eprintln!("Flow from {peer} ended: {e}");
        }
    }

    async fn flow(&self, mut stream: TcpStream, counters: &Counters) -> io::Result<()> {
        stream.set_nodelay(true)?;
        let request = match timeout(self.limits.header_deadline, read_request(&mut stream)).await {
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no request")),
            Ok(Err(ReadError::Io(e))) => return Err(e),
            Ok(Err(ReadError::Unsupported)) => {
                return write_status(&mut stream, Status::Unsupported).await;
            }
            Ok(Ok(request)) => request,
        };
        let Request { kind, target } = request;
        let Some((addr, port)) = target else {
            // Kind::Ping
            return write_status(&mut stream, Status::Ok).await;
        };
        let Ok(_slot) = self.slots.clone().try_acquire_owned() else {
            return write_status(&mut stream, Status::Busy).await;
        };

        match kind {
            Kind::Tcp => {
                let target =
                    match timeout(self.limits.connect_timeout, connect_tcp(&addr, port)).await {
                        Ok(Ok(target)) => target,
                        Ok(Err(status)) => return write_status(&mut stream, status).await,
                        Err(_) => return write_status(&mut stream, Status::TimedOut).await,
                    };
                target.set_nodelay(true)?;
                write_status(&mut stream, Status::Ok).await?;
                let _connected = Connected::new(counters);
                splice(stream, target, counters).await
            }
            Kind::Udp => {
                let udp = match connect_udp(&addr, port).await {
                    Ok(udp) => udp,
                    Err(status) => return write_status(&mut stream, status).await,
                };
                write_status(&mut stream, Status::Ok).await?;
                let _connected = Connected::new(counters);
                let (frames_in, frames_out) = stream.into_split();
                tokio::select! {
                    done = uplink(FrameReader::new(frames_in), &udp, counters) => done,
                    done = downlink(&udp, FrameWriter::new(frames_out), counters) => done,
                }
            }
            Kind::Ping => unreachable!("a ping has no target"),
        }
    }
}

fn status_of(e: &io::Error) -> Status {
    use io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => Status::Refused,
        HostUnreachable | NetworkUnreachable | NetworkDown => Status::Unreachable,
        TimedOut => Status::TimedOut,
        _ => Status::Failed,
    }
}

/// Resolves on the device, literals included, so iOS can synthesise NAT64
/// addresses for IPv4 literals.
async fn resolve(addr: &Addr, port: u16) -> Result<Vec<SocketAddr>, Status> {
    let host = match addr {
        Addr::V4(a) => a.to_string(),
        Addr::V6(a) => a.to_string(),
        Addr::Domain(d) => d.clone(),
    };
    match lookup_host((host.as_str(), port)).await {
        Ok(found) => {
            let found: Vec<_> = found.collect();
            if found.is_empty() {
                Err(Status::NoSuchName)
            } else {
                Ok(found)
            }
        }
        Err(_) => Err(Status::NoSuchName),
    }
}

/// Tries each resolved address in order and reports the last failure.
async fn connect_tcp(addr: &Addr, port: u16) -> Result<TcpStream, Status> {
    let mut last = Status::Failed;
    for address in resolve(addr, port).await? {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last = status_of(&e),
        }
    }
    Err(last)
}

async fn connect_udp(addr: &Addr, port: u16) -> Result<UdpSocket, Status> {
    let address = resolve(addr, port).await?[0];
    let local: SocketAddr = match address {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let udp = UdpSocket::bind(local).await.map_err(|e| status_of(&e))?;
    udp.connect(address).await.map_err(|e| status_of(&e))?;
    Ok(udp)
}

/// Copies one direction, counting bytes, and half-closes the writer at EOF.
async fn copy_counted<R, W>(
    mut from: R,
    mut to: W,
    counter: &std::sync::atomic::AtomicU64,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0; 16 * 1024];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return to.shutdown().await;
        }
        counter.fetch_add(n as u64, Relaxed);
        to.write_all(&buf[..n]).await?;
    }
}

/// Raw bytes both ways. EOF from one side half-closes the other; an error on
/// either ends both.
async fn splice(relay: TcpStream, target: TcpStream, counters: &Counters) -> io::Result<()> {
    let (relay_in, relay_out) = relay.into_split();
    let (target_in, target_out) = target.into_split();
    tokio::try_join!(
        copy_counted(relay_in, target_out, &counters.bytes_up),
        copy_counted(target_in, relay_out, &counters.bytes_down),
    )
    .map(|_| ())
}

/// Raises the soft open-file limit towards `FD_LIMIT` and returns the limit
/// now in force.
fn raise_fd_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 256;
    }
    let want = limit.rlim_max.min(FD_LIMIT as libc::rlim_t);
    if want > limit.rlim_cur {
        let raised = libc::rlimit {
            rlim_cur: want,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return want;
        }
    }
    limit.rlim_cur
}

/// Loopback listeners for `port`: IPv4 and IPv6 where available, at least one.
fn listen_loopback(port: u16) -> io::Result<Vec<std::net::TcpListener>> {
    let v4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port));
    // With port 0 each family gets its own port; with a fixed port they share.
    let v6 = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port));
    match (v4, v6) {
        (Ok(a), Ok(b)) => Ok(vec![a, b]),
        (Ok(a), Err(_)) | (Err(_), Ok(a)) => Ok(vec![a]),
        (Err(e), Err(_)) => Err(e),
    }
}

/// Starts the flow server on loopback `port`, after raising the fd limit.
pub(crate) fn start(port: u16) -> io::Result<ProxyHandle> {
    let limits = FlowLimits::for_fd_limit(raise_fd_limit());
    serve_flows(listen_loopback(port)?, limits)
}

/// Runs the flow server on already bound listeners. Relay tests use this on
/// ephemeral loopback ports.
pub fn serve_flows(
    listeners: Vec<std::net::TcpListener>,
    limits: FlowLimits,
) -> io::Result<ProxyHandle> {
    ProxyHandle::start(listeners, Service::Flows(FlowServer::new(limits)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProxyStats, RUNTIME, flow_start, proxy_stats, proxy_stop};
    use lightning_router_frame::{
        MAX_DATAGRAM,
        flow::{read_status, write_request},
    };
    use tokio::net::TcpListener;

    const PATIENCE: Duration = Duration::from_secs(5);

    fn limits() -> FlowLimits {
        FlowLimits {
            max_flows: 64,
            header_deadline: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(5),
        }
    }

    fn start_server(limits: FlowLimits) -> (ProxyHandle, SocketAddr) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        (serve_flows(vec![listener], limits).unwrap(), address)
    }

    /// A TCP target that echoes, and on EOF writes `bye` before closing.
    fn tcp_echo() -> u16 {
        let listener = RUNTIME
            .block_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        RUNTIME.spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0; 1024];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) => {
                                let _ = s.write_all(b"bye").await;
                                let _ = s.shutdown().await;
                                return;
                            }
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                });
            }
        });
        port
    }

    fn udp_echo() -> u16 {
        let udp = RUNTIME
            .block_on(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let port = udp.local_addr().unwrap().port();
        RUNTIME.spawn(async move {
            let mut buf = vec![0; MAX_DATAGRAM];
            while let Ok((n, from)) = udp.recv_from(&mut buf).await {
                let _ = udp.send_to(&buf[..n], from).await;
            }
        });
        port
    }

    async fn open(server: SocketAddr, request: &Request) -> (TcpStream, Status) {
        let mut stream = TcpStream::connect(server).await.unwrap();
        write_request(&mut stream, request).await.unwrap();
        let status = timeout(PATIENCE, read_status(&mut stream))
            .await
            .expect("no status")
            .unwrap();
        (stream, status)
    }

    async fn read_exactly(stream: &mut TcpStream, len: usize) -> Vec<u8> {
        let mut buf = vec![0; len];
        timeout(PATIENCE, stream.read_exact(&mut buf))
            .await
            .expect("no data")
            .unwrap();
        buf
    }

    async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        timeout(PATIENCE, async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[test]
    fn tcp_flow_reaches_target_by_ip_and_by_name() {
        let (server, address) = start_server(limits());
        let port = tcp_echo();
        RUNTIME.block_on(async {
            for addr in [
                Addr::V4(Ipv4Addr::LOCALHOST),
                Addr::Domain("localhost".into()),
            ] {
                let (mut s, status) = open(address, &Request::tcp(addr, port)).await;
                assert_eq!(status, Status::Ok);
                s.write_all(b"hello").await.unwrap();
                assert_eq!(read_exactly(&mut s, 5).await, b"hello");
            }
        });
        server.stop();
    }

    #[test]
    fn tcp_half_close_carries_the_remaining_reply() {
        let (server, address) = start_server(limits());
        let port = tcp_echo();
        RUNTIME.block_on(async {
            let (mut s, status) =
                open(address, &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), port)).await;
            assert_eq!(status, Status::Ok);
            s.write_all(b"abc").await.unwrap();
            s.shutdown().await.unwrap();
            let mut all = Vec::new();
            timeout(PATIENCE, s.read_to_end(&mut all))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(all, b"abcbye");
        });
        server.stop();
    }

    #[test]
    fn refused_target_replies_refused() {
        let (server, address) = start_server(limits());
        // A port that was just free and is now closed.
        let closed = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (_, status) = RUNTIME.block_on(open(
            address,
            &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), closed),
        ));
        assert_eq!(status, Status::Refused);
        server.stop();
    }

    #[test]
    fn unresolvable_name_replies_no_such_name() {
        let (server, address) = start_server(limits());
        let (_, status) = RUNTIME.block_on(open(
            address,
            &Request::tcp(Addr::Domain("nothing.invalid".into()), 80),
        ));
        assert_eq!(status, Status::NoSuchName);
        server.stop();
    }

    #[test]
    fn unreachable_or_slow_target_fails_within_the_timeout() {
        let (server, address) = start_server(FlowLimits {
            connect_timeout: Duration::from_millis(200),
            ..limits()
        });
        let started = std::time::Instant::now();
        let (_, status) = RUNTIME.block_on(open(
            address,
            &Request::tcp(Addr::V4("192.0.2.1".parse().unwrap()), 80),
        ));
        assert!(
            matches!(status, Status::TimedOut | Status::Unreachable),
            "got {status:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        server.stop();
    }

    #[test]
    fn udp_flow_relays_datagrams_both_ways() {
        let (server, address) = start_server(limits());
        let port = udp_echo();
        RUNTIME.block_on(async {
            let (s, status) =
                open(address, &Request::udp(Addr::V4(Ipv4Addr::LOCALHOST), port)).await;
            assert_eq!(status, Status::Ok);
            let (r, w) = s.into_split();
            let (mut r, mut w) = (FrameReader::new(r), FrameWriter::new(w));
            for datagram in [&b"ping"[..], &[7u8; 9000][..]] {
                w.send(datagram).await.unwrap();
                let got = timeout(PATIENCE, r.next()).await.unwrap().unwrap().unwrap();
                assert_eq!(got, datagram);
            }
        });
        server.stop();
    }

    #[test]
    fn udp_flow_ends_on_an_empty_frame() {
        let (server, address) = start_server(limits());
        let target = RUNTIME
            .block_on(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let port = target.local_addr().unwrap().port();
        RUNTIME.block_on(async {
            let (s, status) =
                open(address, &Request::udp(Addr::V4(Ipv4Addr::LOCALHOST), port)).await;
            assert_eq!(status, Status::Ok);
            let (r, mut raw) = s.into_split();
            let mut r = FrameReader::new(r);
            raw.write_all(&[1, 0, b'x']).await.unwrap();
            let mut buf = [0; 16];
            let (n, flow_addr) = timeout(PATIENCE, target.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"x");

            raw.write_all(&[0, 0]).await.unwrap();
            // The phone hangs up its end of the stream...
            assert!(matches!(
                timeout(PATIENCE, r.next()).await.unwrap(),
                Ok(None) | Err(_)
            ));
            // ...and its UDP socket is closed, so nothing more is sent.
            let _ = target.send_to(b"late", flow_addr).await;
            assert!(
                timeout(Duration::from_millis(200), target.recv_from(&mut buf))
                    .await
                    .is_err()
            );
        });
        server.stop();
    }

    #[test]
    fn ping_replies_ok_and_closes() {
        let (server, address) = start_server(limits());
        RUNTIME.block_on(async {
            let (mut s, status) = open(address, &Request::ping()).await;
            assert_eq!(status, Status::Ok);
            let mut rest = Vec::new();
            timeout(PATIENCE, s.read_to_end(&mut rest))
                .await
                .unwrap()
                .unwrap();
            assert!(rest.is_empty());
        });
        server.stop();
    }

    #[test]
    fn unsupported_request_replies_unsupported() {
        let (server, address) = start_server(limits());
        RUNTIME.block_on(async {
            let mut s = TcpStream::connect(address).await.unwrap();
            s.write_all(&[9, 9]).await.unwrap();
            let status = timeout(PATIENCE, read_status(&mut s))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(status, Status::Unsupported);
        });
        server.stop();
    }

    #[test]
    fn silent_relay_is_dropped_after_the_header_deadline() {
        let (server, address) = start_server(FlowLimits {
            header_deadline: Duration::from_millis(100),
            ..limits()
        });
        RUNTIME.block_on(async {
            let mut s = TcpStream::connect(address).await.unwrap();
            let mut rest = Vec::new();
            timeout(PATIENCE, s.read_to_end(&mut rest))
                .await
                .expect("silent relay was kept")
                .ok();
            assert!(rest.is_empty());
        });
        server.stop();
    }

    #[test]
    fn flow_cap_replies_busy() {
        let (server, address) = start_server(FlowLimits {
            max_flows: 1,
            ..limits()
        });
        let port = tcp_echo();
        RUNTIME.block_on(async {
            let request = Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), port);
            let (first, status) = open(address, &request).await;
            assert_eq!(status, Status::Ok);
            let (_, status) = open(address, &request).await;
            assert_eq!(status, Status::Busy);
            drop(first);
            // The slot comes back once the first flow is gone.
            timeout(PATIENCE, async {
                loop {
                    if open(address, &request).await.1 == Status::Ok {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("slot never freed");
        });
        server.stop();
    }

    #[test]
    fn stats_count_open_flows_and_bytes() {
        let (server, address) = start_server(limits());
        let port = tcp_echo();
        RUNTIME.block_on(async {
            let (_, status) = open(address, &Request::ping()).await;
            assert_eq!(status, Status::Ok);
            assert_eq!(server.stats(), ProxyStats::default());

            let (mut s, status) =
                open(address, &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), port)).await;
            assert_eq!(status, Status::Ok);
            s.write_all(b"12345").await.unwrap();
            read_exactly(&mut s, 5).await;
            assert_eq!(
                server.stats(),
                ProxyStats {
                    connections: 1,
                    bytes_up: 5,
                    bytes_down: 5
                }
            );
            drop(s);
            wait_for("the flow to close", || server.stats().connections == 0).await;
        });
        server.stop();
    }

    #[test]
    fn stop_closes_open_flows_and_frees_the_port() {
        let (server, address) = start_server(limits());
        let port = tcp_echo();
        let mut s = RUNTIME.block_on(async {
            let (mut s, status) =
                open(address, &Request::tcp(Addr::V4(Ipv4Addr::LOCALHOST), port)).await;
            assert_eq!(status, Status::Ok);
            s.write_all(b"a").await.unwrap();
            read_exactly(&mut s, 1).await;
            s
        });

        server.stop();

        let rebound = std::net::TcpListener::bind(address);
        assert!(rebound.is_ok(), "port still held: {rebound:?}");
        let mut rest = Vec::new();
        let after = RUNTIME.block_on(async { timeout(PATIENCE, s.read_to_end(&mut rest)).await });
        assert!(after.is_ok(), "flow was left open");
    }

    #[test]
    fn flow_listener_binds_loopback_only() {
        let listeners = listen_loopback(0).unwrap();
        assert!(!listeners.is_empty());
        for listener in &listeners {
            assert!(listener.local_addr().unwrap().ip().is_loopback());
        }
        let port = listeners[0].local_addr().unwrap().port();
        let host = listeners[0].local_addr().unwrap();
        drop(listeners);

        // A non-loopback address of this host must refuse the flow port.
        let server =
            serve_flows(vec![std::net::TcpListener::bind(host).unwrap()], limits()).unwrap();
        if let Some(lan) = non_loopback_ipv4() {
            assert!(
                std::net::TcpStream::connect_timeout(
                    &(lan, host.port()).into(),
                    Duration::from_secs(1)
                )
                .is_err(),
                "reachable on {lan}"
            );
        }
        let _ = port;
        server.stop();
    }

    /// Any non-loopback IPv4 address of this host, found by routing a UDP
    /// socket towards a public address (nothing is sent).
    fn non_loopback_ipv4() -> Option<Ipv4Addr> {
        let udp = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        udp.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
        match udp.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(a) if !a.is_loopback() && !a.is_unspecified() => Some(a),
            _ => None,
        }
    }

    #[test]
    fn ffi_flow_start_rejects_null_and_stops_cleanly() {
        unsafe {
            assert!(!flow_start(std::ptr::null_mut()));
        }
        let mut proxy = std::ptr::null_mut();
        // The real port may be taken by a running app; only check when bound.
        if unsafe { flow_start(&mut proxy) } {
            assert!(!proxy.is_null());
            assert_eq!(unsafe { proxy_stats(proxy) }, ProxyStats::default());
            unsafe { proxy_stop(proxy) };
        }
    }
}
