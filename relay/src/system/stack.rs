//! Whole-system mode's IP stack glue (spec "Stage B"). `ipstack` turns the
//! utun's packets into TCP streams and UDP flows; each one is answered by
//! fakedns (port 53, any destination) or becomes one phone flow, by name for
//! a fake-IP address (D4) and by IP otherwise. ICMP and anything else that is
//! not TCP or UDP is dropped (D7).
//!
//! `ipstack` completes the TCP handshake before it hands a stream over, so a
//! flow that fails to open can only be closed afterwards: the stream gets a
//! FIN if it is established, and once it is dropped any further segment from
//! the client is answered with a RST.
//!
//! `ipstack` 1.0.1 has no TCP half-close: when the client's FIN arrives with
//! nothing in flight it sends its own FIN at once, so a server that replies
//! after the client shuts down its write side (`nc -N`, some RPC) loses the
//! reply. Server-first close and ordinary request/response work.
//!
//! Dropping an `IpStackTcpStream` calls `block_in_place`, so `run` needs a
//! multi-thread Tokio runtime.

use std::{
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use lightning_router_frame::{
    FrameReader, FrameWriter, MAX_DATAGRAM,
    flow::{Addr, Request},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    task::JoinSet,
    time::{Sleep, timeout},
};

use super::fakedns::{FakeDns, Outcome};
use crate::{
    device::Cache,
    flow::{Stream, open_flow, splice},
};

/// D3: a UDP flow closes after this long with no datagram either way.
pub const UDP_IDLE: Duration = Duration::from_secs(60);
/// D4: where forwarded (non-address) queries go by default.
pub const DEFAULT_UPSTREAM: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53);
const DNS_PORT: u16 = 53;
/// How long a forwarded query waits for the upstream's reply.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a TCP stream whose flow failed may take to send its FIN.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Options {
    /// The resolver forwarded queries go to, through a phone UDP flow.
    pub upstream: SocketAddr,
    /// Whether each packet on the device carries the 4-byte utun header.
    pub packet_information: bool,
    pub mtu: u16,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            upstream: DEFAULT_UPSTREAM,
            packet_information: false,
            mtu: 1500,
        }
    }
}

/// Runs the stack on `device` (one IP packet per read and per write) until
/// the stack stops. Dropping the future ends every flow it opened.
pub async fn run<D>(device: D, cache: Cache, options: Options) -> io::Error
where
    D: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut config = IpStackConfig::default();
    config
        .mtu_unchecked(options.mtu)
        .udp_timeout(UDP_IDLE)
        .packet_information(options.packet_information);
    let mut stack = IpStack::new(config, device);
    let glue = Glue {
        cache,
        dns: Arc::new(Mutex::new(FakeDns::new())),
        upstream: options.upstream,
    };
    let mut flows = JoinSet::new();
    loop {
        tokio::select! {
            accepted = stack.accept() => match accepted {
                Ok(IpStackStream::Tcp(tcp)) => { flows.spawn(glue.clone().tcp(tcp)); }
                Ok(IpStackStream::Udp(udp)) => { flows.spawn(glue.clone().udp(udp)); }
                Ok(_) => {} // D7: ICMP and unparsable packets are dropped
                Err(e) => return io::Error::other(e),
            },
            Some(_) = flows.join_next() => {}
        }
    }
}

#[derive(Clone)]
struct Glue {
    cache: Cache,
    dns: Arc<Mutex<FakeDns>>,
    upstream: SocketAddr,
}

/// Keeps a pool address from being recycled while its flow is open.
struct Pinned {
    dns: Arc<Mutex<FakeDns>>,
    ip: Option<Ipv4Addr>,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        if let Some(ip) = self.ip {
            self.dns.lock().unwrap().unpin(ip);
        }
    }
}

/// 198.18.0.0/15: the router, the DNS server and the fake-IP pool.
fn is_fake(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 198 && (b & 0xfe) == 18
}

fn addr_of(ip: IpAddr) -> Addr {
    match ip {
        IpAddr::V4(v4) => Addr::V4(v4),
        IpAddr::V6(v6) => Addr::V6(v6),
    }
}

impl Glue {
    /// Where a flow to `ip` goes: a fake-IP address by its name (pinned for
    /// the flow's life), any other address as itself. `None` for an
    /// unmapped address in the fake range.
    fn target(&self, ip: IpAddr) -> Option<(Addr, Pinned)> {
        let mut pinned = Pinned {
            dns: self.dns.clone(),
            ip: None,
        };
        match ip {
            IpAddr::V4(v4) if is_fake(v4) => {
                let mut dns = self.dns.lock().unwrap();
                let name = dns.lookup_name(v4)?.to_owned();
                dns.pin(v4);
                pinned.ip = Some(v4);
                Some((Addr::Domain(name), pinned))
            }
            ip => Some((addr_of(ip), pinned)),
        }
    }

    async fn tcp(self, stream: IpStackTcpStream) {
        let dst = stream.peer_addr();
        if dst.port() == DNS_PORT {
            return self.tcp_dns(stream).await;
        }
        let Some((addr, _pinned)) = self.target(dst.ip()) else {
            return refuse(stream).await;
        };
        match open_flow(&self.cache, &Request::tcp(addr, dst.port())).await {
            Ok(mut flow) => {
                let _ = splice(&mut flow, &mut Closing::new(stream)).await;
            }
            Err(_) => refuse(stream).await,
        }
    }

    async fn udp(self, stream: IpStackUdpStream) {
        let dst = stream.peer_addr();
        if dst.port() == DNS_PORT {
            return self.udp_dns(stream).await;
        }
        let Some((addr, _pinned)) = self.target(dst.ip()) else {
            return;
        };
        if let Ok(flow) = open_flow(&self.cache, &Request::udp(addr, dst.port())).await {
            let _ = pump(flow, stream).await;
        }
    }

    /// DNS over TCP: 2-byte big-endian length, then the message. A query
    /// fakedns drops closes the connection.
    async fn tcp_dns(self, mut stream: IpStackTcpStream) {
        loop {
            let mut len = [0; 2];
            if stream.read_exact(&mut len).await.is_err() {
                return;
            }
            let mut query = vec![0; u16::from_be_bytes(len).into()];
            if stream.read_exact(&mut query).await.is_err() {
                return;
            }
            let Some(reply) = self.answer(query).await else {
                return;
            };
            let Ok(len) = u16::try_from(reply.len()) else {
                return;
            };
            let mut message = len.to_be_bytes().to_vec();
            message.extend_from_slice(&reply);
            if stream.write_all(&message).await.is_err() {
                return;
            }
        }
    }

    /// DNS over UDP: one datagram per query. Forwarded queries run alongside,
    /// so a slow upstream does not hold up local answers.
    async fn udp_dns(self, mut stream: IpStackUdpStream) {
        let mut forwards = JoinSet::new();
        let mut datagram = vec![0; MAX_DATAGRAM];
        loop {
            tokio::select! {
                read = stream.read(&mut datagram) => {
                    // 0 is the session closing (or an empty datagram, which
                    // is no query); an error is the idle timeout.
                    let Ok(n @ 1..) = read else { return };
                    let query = datagram[..n].to_vec();
                    let outcome = self.dns.lock().unwrap().respond(&query);
                    match outcome {
                        Outcome::Answer(reply) => { let _ = stream.write(&reply).await; }
                        Outcome::Forward => { forwards.spawn(self.clone().forward(query)); }
                        Outcome::Drop => {}
                    }
                }
                Some(done) = forwards.join_next() => {
                    if let Ok(Some(reply)) = done {
                        let _ = stream.write(&reply).await;
                    }
                }
            }
        }
    }

    async fn answer(&self, query: Vec<u8>) -> Option<Vec<u8>> {
        let outcome = self.dns.lock().unwrap().respond(&query);
        match outcome {
            Outcome::Answer(reply) => Some(reply),
            Outcome::Forward => self.clone().forward(query).await,
            Outcome::Drop => None,
        }
    }

    /// Sends one query to the upstream resolver over its own phone UDP flow
    /// and returns the first reply.
    async fn forward(self, query: Vec<u8>) -> Option<Vec<u8>> {
        let request = Request::udp(addr_of(self.upstream.ip()), self.upstream.port());
        let exchange = async {
            let flow = open_flow(&self.cache, &request).await.ok()?;
            let (flow_in, flow_out) = tokio::io::split(flow);
            FrameWriter::new(flow_out).send(&query).await.ok()?;
            let mut frames = FrameReader::new(flow_in);
            let reply = frames.next().await.ok()??;
            Some(reply.to_vec())
        };
        timeout(FORWARD_TIMEOUT, exchange).await.ok()?
    }
}

/// Ends a TCP stream whose flow could not open: a FIN now if the handshake
/// has finished, and a RST for anything the client sends after the drop.
async fn refuse(stream: IpStackTcpStream) {
    let _ = timeout(CLOSE_TIMEOUT, Closing::new(stream).shutdown()).await;
}

/// Re-polls `poll_shutdown` every `SHUTDOWN_RETRY`. ipstack 1.0.1 parks a
/// shutdown that finds data still unacknowledged and never wakes it when the
/// ACK arrives, so without this the FIN would wait for the session to end.
struct Closing {
    stream: IpStackTcpStream,
    retry: Pin<Box<Sleep>>,
}

const SHUTDOWN_RETRY: Duration = Duration::from_millis(20);

impl Closing {
    fn new(stream: IpStackTcpStream) -> Closing {
        Closing {
            stream,
            retry: Box::pin(tokio::time::sleep(SHUTDOWN_RETRY)),
        }
    }
}

impl AsyncRead for Closing {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Closing {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if let Poll::Ready(done) = Pin::new(&mut self.stream).poll_shutdown(cx) {
                return Poll::Ready(done);
            }
            if self.retry.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            let next = tokio::time::Instant::now() + SHUTDOWN_RETRY;
            self.retry.as_mut().reset(next);
        }
    }
}

/// Moves datagrams between a stack UDP stream and a phone UDP flow until
/// either ends or the stack's idle timeout fires. Each direction has its own
/// loop, because frame reads and writes are not cancel-safe.
async fn pump(flow: Stream, local: IpStackUdpStream) -> io::Result<()> {
    let (flow_in, flow_out) = tokio::io::split(flow);
    let (mut local_in, mut local_out) = tokio::io::split(local);
    let mut frames_out = FrameWriter::new(flow_out);
    let mut frames_in = FrameReader::new(flow_in);
    let uplink = async {
        let mut datagram = vec![0; MAX_DATAGRAM];
        loop {
            // 0 is the session closing; a zero-length frame would end the
            // flow anyway. An error is the idle timeout.
            let n = local_in.read(&mut datagram).await?;
            if n == 0 {
                return Ok(());
            }
            frames_out.send(&datagram[..n]).await?;
        }
    };
    let downlink = async {
        while let Some(datagram) = frames_in.next().await? {
            // One write is one datagram; `write_all` could split it.
            let _ = local_out.write(datagram).await?;
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
    use etherparse::{NetSlice, PacketBuilder, SlicedPacket, TransportSlice};
    use lightning_router::flow::{FlowLimits, serve_flows};
    use std::net::SocketAddrV4;
    use tokio::{
        net::{TcpListener, UdpSocket},
        sync::mpsc,
    };

    const PATIENCE: Duration = Duration::from_secs(5);
    const CLIENT: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 2);
    const TYPE_A: u16 = 1;
    const TYPE_MX: u16 = 15;

    /// A packet device: each read and each write is exactly one packet,
    /// like a utun (a byte duplex could merge two packets into one read).
    struct PacketDevice {
        incoming: mpsc::UnboundedReceiver<Vec<u8>>,
        outgoing: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl AsyncRead for PacketDevice {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.incoming.poll_recv(cx) {
                Poll::Ready(Some(packet)) => {
                    buf.put_slice(&packet);
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl AsyncWrite for PacketDevice {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let _ = self.outgoing.send(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// What came out of the stack, parsed. The TCP addresses are only read
    /// through `Debug`, in assertion messages.
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Seen {
        Tcp {
            src: SocketAddrV4,
            dst: SocketAddrV4,
            seq: u32,
            syn: bool,
            fin: bool,
            rst: bool,
            payload: Vec<u8>,
        },
        Udp {
            src: SocketAddrV4,
            dst: SocketAddrV4,
            payload: Vec<u8>,
        },
        Other,
    }

    fn parse(bytes: &[u8]) -> Seen {
        let packet = SlicedPacket::from_ip(bytes).expect("stack wrote a bad packet");
        let Some(NetSlice::Ipv4(ip)) = &packet.net else {
            return Seen::Other;
        };
        let (from, to) = (ip.header().source_addr(), ip.header().destination_addr());
        match &packet.transport {
            Some(TransportSlice::Tcp(tcp)) => Seen::Tcp {
                src: SocketAddrV4::new(from, tcp.source_port()),
                dst: SocketAddrV4::new(to, tcp.destination_port()),
                seq: tcp.sequence_number(),
                syn: tcp.syn(),
                fin: tcp.fin(),
                rst: tcp.rst(),
                payload: tcp.payload().to_vec(),
            },
            Some(TransportSlice::Udp(udp)) => Seen::Udp {
                src: SocketAddrV4::new(from, udp.source_port()),
                dst: SocketAddrV4::new(to, udp.destination_port()),
                payload: udp.payload().to_vec(),
            },
            _ => Seen::Other,
        }
    }

    /// The stack under test, fed through a `PacketDevice`, with an optional
    /// real phone flow server on loopback behind it.
    struct Net {
        to_stack: mpsc::UnboundedSender<Vec<u8>>,
        from_stack: mpsc::UnboundedReceiver<Vec<u8>>,
        phone: Option<lightning_router::ProxyHandle>,
        next_port: u16,
    }

    impl Net {
        fn start(with_phone: bool, upstream: SocketAddr) -> Net {
            let cache = Cache::new();
            let phone = with_phone.then(|| {
                let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                cache.set(State::Ready(Device::Addr(listener.local_addr().unwrap())));
                let limits = FlowLimits {
                    max_flows: 64,
                    header_deadline: Duration::from_secs(2),
                    connect_timeout: Duration::from_secs(5),
                };
                serve_flows(vec![listener], limits).unwrap()
            });
            let (to_stack, incoming) = mpsc::unbounded_channel();
            let (outgoing, from_stack) = mpsc::unbounded_channel();
            let device = PacketDevice { incoming, outgoing };
            let options = Options {
                upstream,
                ..Options::default()
            };
            tokio::spawn(run(device, cache, options));
            Net {
                to_stack,
                from_stack,
                phone,
                next_port: 40_000,
            }
        }

        fn port(&mut self) -> u16 {
            self.next_port += 1;
            self.next_port
        }

        async fn stop(self) {
            if let Some(phone) = self.phone {
                tokio::task::spawn_blocking(move || phone.stop())
                    .await
                    .unwrap();
            }
        }

        fn send(&self, packet: Vec<u8>) {
            self.to_stack.send(packet).unwrap();
        }

        async fn recv(&mut self) -> Seen {
            let bytes = timeout(PATIENCE, self.from_stack.recv())
                .await
                .expect("stack sent nothing")
                .unwrap();
            parse(&bytes)
        }

        async fn recv_within(&mut self, wait: Duration) -> Option<Seen> {
            timeout(wait, self.from_stack.recv())
                .await
                .ok()
                .map(|b| parse(&b.unwrap()))
        }

        fn send_udp(&self, src: SocketAddrV4, dst: SocketAddrV4, payload: &[u8]) {
            let builder = PacketBuilder::ipv4(src.ip().octets(), dst.ip().octets(), 64)
                .udp(src.port(), dst.port());
            let mut packet = Vec::new();
            builder.write(&mut packet, payload).unwrap();
            self.send(packet);
        }

        async fn recv_udp(&mut self) -> (SocketAddrV4, SocketAddrV4, Vec<u8>) {
            loop {
                if let Seen::Udp { src, dst, payload } = self.recv().await {
                    return (src, dst, payload);
                }
            }
        }

        /// Resolves `name` through the stack's DNS (UDP to the DNS address).
        async fn resolve(&mut self, name: &str) -> Ipv4Addr {
            let src = SocketAddrV4::new(CLIENT, self.port());
            let dns = SocketAddrV4::new(super::super::fakedns::DNS_ADDR, DNS_PORT);
            self.send_udp(src, dns, &dns_query(7, name, TYPE_A));
            let (_, _, reply) = self.recv_udp().await;
            answer_ip(&reply)
        }

        /// Runs the client side of a TCP handshake to `dst`.
        async fn connect(&mut self, dst: SocketAddrV4) -> Client {
            let mut client = Client {
                src: SocketAddrV4::new(CLIENT, self.port()),
                dst,
                seq: 1000,
                ack: 0,
            };
            self.send(client.segment(Flags::Syn, &[]));
            client.seq += 1;
            match self.recv().await {
                Seen::Tcp { syn: true, seq, .. } => client.ack = seq.wrapping_add(1),
                other => panic!("expected SYN-ACK, got {other:?}"),
            }
            self.send(client.segment(Flags::Ack, &[]));
            client
        }

        fn write(&self, client: &mut Client, data: &[u8]) {
            self.send(client.segment(Flags::Data, data));
            client.seq += data.len() as u32;
        }

        fn fin(&self, client: &mut Client) {
            self.send(client.segment(Flags::Fin, &[]));
            client.seq += 1;
        }

        /// Reads the server's bytes until `want` have arrived or it sends
        /// FIN (acknowledged), whichever is first. Returns (bytes, fin).
        async fn read(&mut self, client: &mut Client, want: usize) -> (Vec<u8>, bool) {
            let mut data = Vec::new();
            loop {
                let Seen::Tcp {
                    seq,
                    fin,
                    rst,
                    payload,
                    ..
                } = self.recv().await
                else {
                    continue;
                };
                assert!(!rst, "unexpected RST");
                if seq == client.ack {
                    client.ack = client.ack.wrapping_add(payload.len() as u32);
                    data.extend_from_slice(&payload);
                    if fin {
                        client.ack = client.ack.wrapping_add(1);
                    }
                }
                self.send(client.segment(Flags::Ack, &[]));
                if fin || data.len() >= want {
                    return (data, fin);
                }
            }
        }

        /// Keeps sending data until the stack resets the connection; fails
        /// if any payload comes back or no RST arrives in time.
        async fn expect_reset(&mut self, client: &mut Client) {
            let deadline = tokio::time::Instant::now() + PATIENCE;
            while tokio::time::Instant::now() < deadline {
                self.send(client.segment(Flags::Data, b"x"));
                while let Some(seen) = self.recv_within(Duration::from_millis(200)).await {
                    match seen {
                        Seen::Tcp { rst: true, .. } => return,
                        Seen::Tcp { payload, .. } => {
                            assert!(payload.is_empty(), "refused flow sent data")
                        }
                        _ => {}
                    }
                }
            }
            panic!("no RST within {PATIENCE:?}");
        }
    }

    enum Flags {
        Syn,
        Ack,
        Data,
        Fin,
    }

    struct Client {
        src: SocketAddrV4,
        dst: SocketAddrV4,
        seq: u32,
        ack: u32,
    }

    impl Client {
        fn segment(&self, flags: Flags, payload: &[u8]) -> Vec<u8> {
            let builder = PacketBuilder::ipv4(self.src.ip().octets(), self.dst.ip().octets(), 64)
                .tcp(self.src.port(), self.dst.port(), self.seq, 65_535);
            let builder = match flags {
                Flags::Syn => builder.syn(),
                Flags::Ack => builder.ack(self.ack),
                Flags::Data => builder.ack(self.ack).psh(),
                Flags::Fin => builder.ack(self.ack).fin(),
            };
            let mut packet = Vec::new();
            builder.write(&mut packet, payload).unwrap();
            packet
        }
    }

    fn dns_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    /// The address in a one-answer A reply (its last four bytes).
    fn answer_ip(reply: &[u8]) -> Ipv4Addr {
        assert_eq!(&reply[6..8], &[0, 1], "expected one answer: {reply:?}");
        let ip: [u8; 4] = reply[reply.len() - 4..].try_into().unwrap();
        Ipv4Addr::from(ip)
    }

    fn unused_upstream() -> SocketAddr {
        (Ipv4Addr::LOCALHOST, 9).into()
    }

    /// A loopback TCP target that echoes the first `n` bytes back.
    async fn echo_target(n: usize) -> u16 {
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = target.accept().await.unwrap();
            let mut buf = vec![0; n];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
            let _ = s.read(&mut [0; 1]).await;
        });
        port
    }

    fn closed_port() -> u16 {
        let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn syn_to_pool_address_opens_tcp_flow_by_name() {
        let mut net = Net::start(true, unused_upstream());
        let port = echo_target(5).await;
        let ip = net.resolve("localhost").await;
        assert!(is_fake(ip), "{ip} is not a pool address");
        // The pool address routes nowhere; only the name reaches the target.
        let mut client = net.connect(SocketAddrV4::new(ip, port)).await;
        net.write(&mut client, b"hello");
        let (back, _) = net.read(&mut client, 5).await;
        assert_eq!(back, b"hello");
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn syn_to_real_address_opens_tcp_flow_by_ip() {
        let mut net = Net::start(true, unused_upstream());
        let port = echo_target(5).await;
        let mut client = net
            .connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await;
        net.write(&mut client, b"hello");
        let (back, _) = net.read(&mut client, 5).await;
        assert_eq!(back, b"hello");
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_payload_and_fin_cross_both_ways() {
        let mut net = Net::start(true, unused_upstream());
        let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = target.local_addr().unwrap().port();
        // The server closes first. ipstack 1.0.1 answers a client FIN with
        // its own FIN at once (no half-close), so a reply sent after the
        // client's FIN would be lost; see the module docs.
        let server = tokio::spawn(async move {
            let (mut s, _) = target.accept().await.unwrap();
            let mut got = [0; 4];
            s.read_exact(&mut got).await.unwrap();
            s.write_all(b"bye").await.unwrap();
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap(); // ends at the client's FIN
            (got, rest)
        });

        let mut client = net
            .connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await;
        net.write(&mut client, b"data");
        let (back, fin) = net.read(&mut client, usize::MAX).await;
        assert_eq!(back, b"bye");
        assert!(fin, "the server's FIN did not arrive");
        net.fin(&mut client);
        let (got, rest) = timeout(PATIENCE, server).await.unwrap().unwrap();
        assert_eq!(&got, b"data");
        assert!(rest.is_empty());
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_flow_resets_the_client() {
        let mut net = Net::start(true, unused_upstream());
        let mut client = net
            .connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, closed_port()))
            .await;
        net.expect_reset(&mut client).await;
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn syn_to_unmapped_pool_address_resets() {
        // No phone at all: an unmapped address must not even try a flow.
        let mut net = Net::start(false, unused_upstream());
        let mut client = net
            .connect(SocketAddrV4::new(Ipv4Addr::new(198, 19, 200, 200), 80))
            .await;
        net.expect_reset(&mut client).await;
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn udp_to_port_53_anywhere_is_answered_locally() {
        // No phone: the answers can only come from the Mac.
        let mut net = Net::start(false, unused_upstream());
        let src = SocketAddrV4::new(CLIENT, 5353);
        let resolver = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53);
        net.send_udp(src, resolver, &dns_query(42, "example.com", TYPE_A));
        let (from, to, reply) = net.recv_udp().await;
        assert_eq!((from, to), (resolver, src));
        assert_eq!(&reply[..2], &42u16.to_be_bytes());
        let ip = answer_ip(&reply);
        assert!(is_fake(ip), "{ip} is not a pool address");

        // TCP to port 53 at another address, with the length prefix.
        let mut client = net
            .connect(SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 53))
            .await;
        let query = dns_query(43, "example.com", TYPE_A);
        let mut message = (query.len() as u16).to_be_bytes().to_vec();
        message.extend_from_slice(&query);
        net.write(&mut client, &message);
        let (back, _) = net.read(&mut client, 2).await;
        let len = u16::from_be_bytes([back[0], back[1]]) as usize;
        let mut back = back;
        if back.len() < 2 + len {
            let (more, _) = net.read(&mut client, 2 + len - back.len()).await;
            back.extend_from_slice(&more);
        }
        assert_eq!(
            answer_ip(&back[2..]),
            ip,
            "the same name maps to one address"
        );
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn udp_to_pool_address_opens_udp_flow_by_name() {
        let mut net = Net::start(true, unused_upstream());
        // "localhost" may resolve to either family on the phone side.
        let echo4 = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = echo4.local_addr().unwrap().port();
        let echo6 = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, port))
            .await
            .ok();
        for echo in std::iter::once(echo4).chain(echo6) {
            tokio::spawn(async move {
                let mut buf = [0; 64];
                let (n, from) = echo.recv_from(&mut buf).await.unwrap();
                echo.send_to(&buf[..n], from).await.unwrap();
            });
        }

        let ip = net.resolve("localhost").await;
        let src = SocketAddrV4::new(CLIENT, net.port());
        let dst = SocketAddrV4::new(ip, port);
        net.send_udp(src, dst, b"ping");
        let (from, to, payload) = net.recv_udp().await;
        assert_eq!((from, to), (dst, src));
        assert_eq!(payload, b"ping");
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwarded_dns_query_goes_to_upstream_via_udp_flow() {
        let upstream = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let mut net = Net::start(true, upstream.local_addr().unwrap());
        let query = dns_query(99, "gmail.com", TYPE_MX);
        let fake_reply = b"\x00\x63upstream says hi".to_vec();
        let resolver = {
            let (query, reply) = (query.clone(), fake_reply.clone());
            tokio::spawn(async move {
                let mut buf = [0; 512];
                let (n, from) = upstream.recv_from(&mut buf).await.unwrap();
                assert_eq!(
                    &buf[..n],
                    &query[..],
                    "the query was not forwarded unchanged"
                );
                upstream.send_to(&reply, from).await.unwrap();
            })
        };

        let src = SocketAddrV4::new(CLIENT, 5300);
        let dst = SocketAddrV4::new(Ipv4Addr::new(8, 8, 4, 4), 53);
        net.send_udp(src, dst, &query);
        let (from, to, reply) = net.recv_udp().await;
        assert_eq!((from, to), (dst, src));
        assert_eq!(reply, fake_reply);
        timeout(PATIENCE, resolver).await.unwrap().unwrap();
        net.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn icmp_is_dropped_and_the_stack_keeps_working() {
        let mut net = Net::start(false, unused_upstream());
        let mut ping = Vec::new();
        PacketBuilder::ipv4(CLIENT.octets(), [1, 1, 1, 1], 64)
            .icmpv4_echo_request(1, 1)
            .write(&mut ping, b"are you there")
            .unwrap();
        net.send(ping);
        assert!(
            net.recv_within(Duration::from_millis(300)).await.is_none(),
            "the stack answered a ping"
        );

        let ip = net.resolve("still.works").await;
        assert!(is_fake(ip));
        net.stop().await;
    }

    #[test]
    fn fake_range_is_198_18_slash_15() {
        assert!(is_fake(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_fake(Ipv4Addr::new(198, 19, 255, 255)));
        assert!(!is_fake(Ipv4Addr::new(198, 20, 0, 0)));
        assert!(!is_fake(Ipv4Addr::new(198, 17, 255, 255)));
    }
}
