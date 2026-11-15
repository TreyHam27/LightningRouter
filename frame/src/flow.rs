//! The flow-port protocol: one usbmuxd connection per flow.
//!
//! The relay sends a [`Request`]; the phone answers with one [`Status`] byte.
//! All integers are little-endian. After `Status::Ok`, a TCP flow carries raw
//! bytes and a UDP flow carries frames (see the crate root).

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VERSION: u8 = 1;

/// What the relay asks the phone to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ping,
    Tcp,
    Udp,
}

/// Where a flow goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Addr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    /// ASCII, 1 to 255 bytes.
    Domain(String),
}

/// A request to open a flow. `target` is `None` exactly for `Kind::Ping`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub kind: Kind,
    pub target: Option<(Addr, u16)>,
}

impl Request {
    pub fn ping() -> Self {
        Self { kind: Kind::Ping, target: None }
    }
    pub fn tcp(addr: Addr, port: u16) -> Self {
        Self { kind: Kind::Tcp, target: Some((addr, port)) }
    }
    pub fn udp(addr: Addr, port: u16) -> Self {
        Self { kind: Kind::Udp, target: Some((addr, port)) }
    }
}

/// Why `read_request` failed.
#[derive(Debug)]
pub enum ReadError {
    /// Unknown version, kind or address type, or a bad domain. The phone
    /// replies `Status::Unsupported`.
    Unsupported,
    /// The stream failed or ended mid-header.
    Io(io::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("unsupported flow request"),
            Self::Io(e) => write!(f, "flow request: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The phone's one-byte reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    Failed = 1,
    Refused = 2,
    Unreachable = 3,
    NoSuchName = 4,
    TimedOut = 5,
    Unsupported = 6,
    Busy = 7,
}

impl Status {
    pub const ALL: [Status; 8] = [
        Status::Ok,
        Status::Failed,
        Status::Refused,
        Status::Unreachable,
        Status::NoSuchName,
        Status::TimedOut,
        Status::Unsupported,
        Status::Busy,
    ];

    /// An unknown byte decodes as `Failed`.
    pub fn from_byte(byte: u8) -> Self {
        Self::ALL.get(usize::from(byte)).copied().unwrap_or(Status::Failed)
    }
}

/// Writes a request as a single write.
///
/// Fails with `InvalidInput` for a domain that is empty, longer than 255
/// bytes, or not ASCII, or for a PING/non-PING whose target does not match.
pub async fn write_request<W: AsyncWrite + Unpin>(w: &mut W, req: &Request) -> io::Result<()> {
    let bad = |m| io::Error::new(io::ErrorKind::InvalidInput, m);
    let mut buf = vec![VERSION];
    match (req.kind, &req.target) {
        (Kind::Ping, None) => buf.push(0),
        (Kind::Ping, Some(_)) | (Kind::Tcp | Kind::Udp, None) => {
            return Err(bad("target does not match request kind"));
        }
        (kind, Some((addr, port))) => {
            buf.push(if kind == Kind::Tcp { 1 } else { 2 });
            match addr {
                Addr::V4(a) => {
                    buf.push(1);
                    buf.extend_from_slice(&a.octets());
                }
                Addr::Domain(d) => {
                    if d.is_empty() || d.len() > 255 || !d.is_ascii() {
                        return Err(bad("domain must be 1 to 255 ASCII bytes"));
                    }
                    buf.push(3);
                    buf.push(d.len() as u8);
                    buf.extend_from_slice(d.as_bytes());
                }
                Addr::V6(a) => {
                    buf.push(4);
                    buf.extend_from_slice(&a.octets());
                }
            }
            buf.extend_from_slice(&port.to_le_bytes());
        }
    }
    w.write_all(&buf).await
}

/// Reads a request. Not cancel-safe: wrap the whole call in a deadline and
/// drop the connection on expiry.
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Request, ReadError> {
    if r.read_u8().await? != VERSION {
        return Err(ReadError::Unsupported);
    }
    let kind = match r.read_u8().await? {
        0 => return Ok(Request::ping()),
        1 => Kind::Tcp,
        2 => Kind::Udp,
        _ => return Err(ReadError::Unsupported),
    };
    let addr = match r.read_u8().await? {
        1 => {
            let mut b = [0; 4];
            r.read_exact(&mut b).await?;
            Addr::V4(b.into())
        }
        3 => {
            let len = usize::from(r.read_u8().await?);
            let mut b = vec![0; len];
            r.read_exact(&mut b).await?;
            if len == 0 || !b.is_ascii() {
                return Err(ReadError::Unsupported);
            }
            Addr::Domain(String::from_utf8(b).map_err(|_| ReadError::Unsupported)?)
        }
        4 => {
            let mut b = [0; 16];
            r.read_exact(&mut b).await?;
            Addr::V6(b.into())
        }
        _ => return Err(ReadError::Unsupported),
    };
    let mut port = [0; 2];
    r.read_exact(&mut port).await?;
    Ok(Request { kind, target: Some((addr, u16::from_le_bytes(port))) })
}

pub async fn write_status<W: AsyncWrite + Unpin>(w: &mut W, status: Status) -> io::Result<()> {
    w.write_all(&[status as u8]).await
}

pub async fn read_status<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Status> {
    Ok(Status::from_byte(r.read_u8().await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn encode(req: &Request) -> Vec<u8> {
        let mut wire = Vec::new();
        write_request(&mut wire, req).await.unwrap();
        wire
    }

    fn all_requests() -> Vec<Request> {
        let v6 = Addr::V6("2001:db8::1".parse().unwrap());
        let v4 = Addr::V4("192.0.2.7".parse().unwrap());
        let dom = Addr::Domain("example.com".into());
        let mut out = vec![Request::ping()];
        for a in [v4, v6, dom] {
            out.push(Request::tcp(a.clone(), 443));
            out.push(Request::udp(a, 53));
        }
        out
    }

    #[tokio::test]
    async fn request_wire_layout_is_exact() {
        let wire = encode(&Request::tcp(Addr::Domain("example.com".into()), 443)).await;
        let mut want = vec![1, 1, 3, 11];
        want.extend_from_slice(b"example.com");
        want.extend_from_slice(&[0xbb, 0x01]);
        assert_eq!(wire, want);
        assert_eq!(encode(&Request::ping()).await, [1, 0]);
    }

    #[tokio::test]
    async fn request_round_trips_every_kind_and_address() {
        for req in all_requests() {
            let wire = encode(&req).await;
            assert_eq!(read_request(&mut wire.as_slice()).await.unwrap(), req);
        }
    }

    #[tokio::test]
    async fn request_reassembles_a_byte_at_a_time() {
        for req in all_requests() {
            let (mut near, mut far) = duplex(1);
            let sent = req.clone();
            let sender = tokio::spawn(async move { write_request(&mut near, &sent).await.unwrap() });
            assert_eq!(read_request(&mut far).await.unwrap(), req);
            sender.await.unwrap();
        }
    }

    #[tokio::test]
    async fn unknown_version_kind_or_atyp_is_unsupported() {
        for wire in [&[2u8, 1, 1, 0, 0, 0, 0, 0, 0][..], &[1, 9], &[1, 1, 2, 0, 0, 0, 0, 0, 0]] {
            let got = read_request(&mut &wire[..]).await;
            assert!(matches!(got, Err(ReadError::Unsupported)), "{wire:?}");
        }
    }

    #[tokio::test]
    async fn empty_or_non_ascii_domain_is_unsupported() {
        for wire in [&[1u8, 1, 3, 0, 0x50, 0][..], &[1, 1, 3, 2, 0xc3, 0xa9, 0x50, 0]] {
            let got = read_request(&mut &wire[..]).await;
            assert!(matches!(got, Err(ReadError::Unsupported)), "{wire:?}");
        }
    }

    #[tokio::test]
    async fn domain_over_255_bytes_is_rejected_on_encode() {
        let mut wire = Vec::new();
        let long = Request::tcp(Addr::Domain("a".repeat(256)), 80);
        let err = write_request(&mut wire, &long).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(wire.is_empty());
        let max = Request::tcp(Addr::Domain("a".repeat(255)), 80);
        write_request(&mut wire, &max).await.unwrap();
    }

    #[tokio::test]
    async fn truncated_request_is_an_io_error() {
        let wire = encode(&Request::tcp(Addr::Domain("example.com".into()), 443)).await;
        for cut in 0..wire.len() {
            let got = read_request(&mut &wire[..cut]).await;
            assert!(matches!(got, Err(ReadError::Io(_))), "cut at {cut}");
        }
    }

    #[tokio::test]
    async fn status_round_trips_every_value() {
        for status in Status::ALL {
            let mut wire = Vec::new();
            write_status(&mut wire, status).await.unwrap();
            assert_eq!(wire, [status as u8]);
            assert_eq!(read_status(&mut wire.as_slice()).await.unwrap(), status);
        }
        assert_eq!(Status::from_byte(200), Status::Failed);
    }
}
