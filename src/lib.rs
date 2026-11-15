// Jackson Coxson

//! The device end of the link: accepts the Mac relay over TCP and exchanges
//! its framed datagrams with a UDP target. Swift drives it through the three
//! `proxy_*` functions at the bottom of this file.

use std::{
    ffi::{CStr, c_char},
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

use lightning_router_frame::{DEVICE_PORT, FrameReader, FrameWriter, MAX_DATAGRAM};
use tokio::{
    net::{
        TcpListener, TcpStream, UdpSocket, lookup_host,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    runtime::{self, Runtime},
    sync::oneshot,
    task::{JoinHandle, JoinSet},
};

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("failed to start the Tokio runtime")
});

/// A snapshot of a running proxy.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProxyStats {
    /// Relays connected right now.
    pub connections: u64,
    /// Datagram bytes sent towards the target since the proxy started.
    pub bytes_up: u64,
    /// Datagram bytes received from the target since the proxy started.
    pub bytes_down: u64,
}

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
}

/// Counts one relay as connected for as long as it lives.
struct Connected<'a>(&'a Counters);

impl<'a> Connected<'a> {
    fn new(counters: &'a Counters) -> Self {
        counters.connections.fetch_add(1, Relaxed);
        Self(counters)
    }
}

impl Drop for Connected<'_> {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Relaxed);
    }
}

struct Target {
    host: String,
    port: u16,
}

/// A running proxy. Created by `proxy_start`, destroyed by `proxy_stop`.
pub struct ProxyHandle {
    shutdown: oneshot::Sender<()>,
    accept_loop: JoinHandle<()>,
    counters: Arc<Counters>,
}

impl ProxyHandle {
    fn start(listener: std::net::TcpListener, target: Target) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        let listener = {
            let _runtime = RUNTIME.enter();
            TcpListener::from_std(listener)?
        };
        let (shutdown, stopped) = oneshot::channel();
        let counters = Arc::new(Counters::default());
        let accept_loop = RUNTIME.spawn(accept_loop(
            listener,
            Arc::new(target),
            counters.clone(),
            stopped,
        ));
        Ok(Self {
            shutdown,
            accept_loop,
            counters,
        })
    }

    fn stats(&self) -> ProxyStats {
        ProxyStats {
            connections: self.counters.connections.load(Relaxed),
            bytes_up: self.counters.bytes_up.load(Relaxed),
            bytes_down: self.counters.bytes_down.load(Relaxed),
        }
    }

    /// Returns once the listening port is free again, so a proxy can be
    /// restarted straight away. Must be called from outside the runtime.
    fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = RUNTIME.block_on(self.accept_loop);
    }
}

/// Listens for the relay on every interface. usbmuxd reaches the device over
/// IPv4 or IPv6 depending on the connection; an IPv6 wildcard socket takes
/// both on Apple platforms.
fn listen(port: u16) -> io::Result<std::net::TcpListener> {
    std::net::TcpListener::bind((Ipv6Addr::UNSPECIFIED, port))
        .or_else(|_| std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)))
}

async fn accept_loop(
    listener: TcpListener,
    target: Arc<Target>,
    counters: Arc<Counters>,
    mut stopped: oneshot::Receiver<()>,
) {
    // Dropping the set on the way out aborts every session still running.
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            Some(_) = sessions.join_next() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, relay)) => {
                    let target = target.clone();
                    let counters = counters.clone();
                    sessions.spawn(async move {
                        let _connected = Connected::new(&counters);
                        match session(stream, &target, &counters).await {
                            Ok(()) => println!("Relay {relay} disconnected"),
                            Err(e) => eprintln!("Relay {relay} dropped: {e}"),
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Failed to accept a relay: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
}

/// Serves one relay until either side of it fails or the relay hangs up.
async fn session(stream: TcpStream, target: &Target, counters: &Counters) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let udp = connect_target(target).await?;
    let (frames_in, frames_out) = stream.into_split();

    // Each direction runs in its own loop, so neither waits on the other and
    // neither is cancelled mid-frame while the session is healthy.
    tokio::select! {
        done = uplink(FrameReader::new(frames_in), &udp, counters) => done,
        done = downlink(&udp, FrameWriter::new(frames_out), counters) => done,
    }
}

/// Resolved per session, so a hostname target follows DNS changes and the
/// socket follows the device's current route.
async fn connect_target(target: &Target) -> io::Result<UdpSocket> {
    let address = lookup_host((target.host.as_str(), target.port))
        .await?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "target did not resolve"))?;
    let local: SocketAddr = match address {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let udp = UdpSocket::bind(local).await?;
    udp.connect(address).await?;
    Ok(udp)
}

async fn uplink(
    mut frames: FrameReader<OwnedReadHalf>,
    udp: &UdpSocket,
    counters: &Counters,
) -> io::Result<()> {
    while let Some(datagram) = frames.next().await? {
        counters.bytes_up.fetch_add(datagram.len() as u64, Relaxed);
        udp.send(datagram).await?;
    }
    Ok(())
}

async fn downlink(
    udp: &UdpSocket,
    mut frames: FrameWriter<OwnedWriteHalf>,
    counters: &Counters,
) -> io::Result<()> {
    let mut datagram = vec![0; MAX_DATAGRAM];
    loop {
        let len = udp.recv(&mut datagram).await?;
        counters.bytes_down.fetch_add(len as u64, Relaxed);
        frames.send(&datagram[..len]).await?;
    }
}

/// Starts a proxy that accepts the relay on the device port and forwards its
/// datagrams to UDP `address:port`. `address` is an IP address or a hostname.
///
/// On success writes the new proxy to `*proxy` and returns true. Returns
/// false, leaving `*proxy` alone, if an argument is invalid or the device
/// port cannot be bound.
///
/// # Safety
/// `address` must be null or point to a NUL-terminated string. `proxy` must
/// be null or valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn proxy_start(
    address: *const c_char,
    port: u16,
    proxy: *mut *mut ProxyHandle,
) -> bool {
    if address.is_null() || proxy.is_null() {
        return false;
    }
    let Ok(host) = unsafe { CStr::from_ptr(address) }.to_str() else {
        return false;
    };
    let target = Target {
        host: host.to_owned(),
        port,
    };

    match listen(DEVICE_PORT).and_then(|listener| ProxyHandle::start(listener, target)) {
        Ok(handle) => {
            unsafe { *proxy = Box::into_raw(Box::new(handle)) };
            true
        }
        Err(e) => {
            eprintln!("Failed to listen on port {DEVICE_PORT}: {e}");
            false
        }
    }
}

/// Reads the proxy's counters. A null `proxy` reads as all zeroes.
///
/// # Safety
/// `proxy` must be null or a live proxy from `proxy_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn proxy_stats(proxy: *const ProxyHandle) -> ProxyStats {
    match unsafe { proxy.as_ref() } {
        Some(handle) => handle.stats(),
        None => ProxyStats::default(),
    }
}

/// Stops the proxy, drops its relays and frees it. The device port is free
/// again when this returns.
///
/// # Safety
/// `proxy` must be null or a live proxy from `proxy_start`, which must not be
/// used again afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn proxy_stop(proxy: *mut ProxyHandle) {
    if proxy.is_null() {
        return;
    }
    unsafe { Box::from_raw(proxy) }.stop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    const PATIENCE: Duration = Duration::from_secs(5);

    /// A UDP socket that sends every datagram straight back.
    fn echo_target() -> Target {
        let udp = RUNTIME
            .block_on(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let port = udp.local_addr().unwrap().port();
        RUNTIME.spawn(async move {
            let mut datagram = vec![0; MAX_DATAGRAM];
            while let Ok((len, from)) = udp.recv_from(&mut datagram).await {
                let _ = udp.send_to(&datagram[..len], from).await;
            }
        });
        Target {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }
    }

    fn start_proxy() -> (ProxyHandle, SocketAddr) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let proxy = ProxyHandle::start(listener, echo_target()).unwrap();
        (proxy, address)
    }

    type Relay = (FrameReader<OwnedReadHalf>, FrameWriter<OwnedWriteHalf>);

    async fn connect_relay(address: SocketAddr) -> Relay {
        let (reader, writer) = TcpStream::connect(address).await.unwrap().into_split();
        (FrameReader::new(reader), FrameWriter::new(writer))
    }

    async fn exchange(relay: &mut Relay, datagram: &[u8]) -> Vec<u8> {
        relay.1.send(datagram).await.unwrap();
        timeout(PATIENCE, relay.0.next())
            .await
            .expect("no reply from the target")
            .unwrap()
            .expect("proxy hung up")
            .to_vec()
    }

    #[test]
    fn relays_datagrams_to_the_target_and_back() {
        let (proxy, address) = start_proxy();
        let largest = vec![0x5a; 9000];

        RUNTIME.block_on(async {
            let mut relay = connect_relay(address).await;
            assert_eq!(exchange(&mut relay, b"ping").await, b"ping");
            assert_eq!(exchange(&mut relay, &largest).await, largest);
        });

        proxy.stop();
    }

    #[test]
    fn stats_count_connections_and_bytes() {
        let (proxy, address) = start_proxy();
        assert_eq!(proxy.stats(), ProxyStats::default());

        let relay = RUNTIME.block_on(async {
            let mut relay = connect_relay(address).await;
            exchange(&mut relay, b"12345").await;
            exchange(&mut relay, b"678").await;
            relay
        });
        assert_eq!(
            proxy.stats(),
            ProxyStats {
                connections: 1,
                bytes_up: 8,
                bytes_down: 8,
            }
        );

        drop(relay);
        RUNTIME.block_on(async {
            timeout(PATIENCE, async {
                while proxy.stats().connections != 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the hung-up relay was still counted");
        });
        proxy.stop();
    }

    #[test]
    fn each_relay_gets_its_own_path_to_the_target() {
        let (proxy, address) = start_proxy();

        RUNTIME.block_on(async {
            let mut first = connect_relay(address).await;
            let mut second = connect_relay(address).await;
            assert_eq!(exchange(&mut second, b"two").await, b"two");
            assert_eq!(exchange(&mut first, b"one").await, b"one");
        });

        proxy.stop();
    }

    #[test]
    fn stop_hangs_up_relays_and_frees_the_port_at_once() {
        let (proxy, address) = start_proxy();
        let mut relay = RUNTIME.block_on(async {
            let mut relay = connect_relay(address).await;
            exchange(&mut relay, b"up").await;
            relay
        });

        proxy.stop();

        let restarted = std::net::TcpListener::bind(address);
        assert!(restarted.is_ok(), "port still held: {restarted:?}");
        let after = RUNTIME
            .block_on(async { timeout(PATIENCE, relay.0.next()).await })
            .expect("relay was left connected");
        assert!(matches!(after, Ok(None) | Err(_)), "got {after:?}");
    }

    #[test]
    fn listens_for_ipv4_and_ipv6_relays() {
        let listener = listen(0).unwrap();
        let port = listener.local_addr().unwrap().port();

        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("IPv4 relay refused");
        std::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port)).expect("IPv6 relay refused");
    }

    #[test]
    fn ffi_rejects_null_arguments() {
        let mut proxy = std::ptr::null_mut();
        unsafe {
            assert!(!proxy_start(std::ptr::null(), 1, &mut proxy));
            assert!(!proxy_start(c"127.0.0.1".as_ptr(), 1, std::ptr::null_mut()));
            assert_eq!(proxy_stats(std::ptr::null()), ProxyStats::default());
            proxy_stop(std::ptr::null_mut());
        }
        assert!(proxy.is_null());
    }
}
