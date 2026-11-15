//! The link between the Mac relay and the app on the device.
//!
//! UDP datagrams cross the usbmuxd TCP stream as frames: a `u16`
//! little-endian length followed by that many bytes. A zero-length frame, or
//! the stream ending on a frame boundary, means the peer is done.

pub mod flow;

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// TCP port the app listens on, on the device.
pub const DEVICE_PORT: u16 = 51820;
/// UDP port the relay listens on, on the Mac.
pub const RELAY_PORT: u16 = 3400;
/// TCP port the app's flow server listens on, on the device (serverless mode).
pub const FLOW_PORT: u16 = 51821;
/// Largest datagram a frame can carry.
pub const MAX_DATAGRAM: usize = u16::MAX as usize;

const HEADER: usize = size_of::<u16>();

/// Reads frames off a stream.
pub struct FrameReader<R> {
    inner: BufReader<R>,
    datagram: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            // Sized for one full frame, so a burst of small frames costs one
            // read syscall rather than two per frame.
            inner: BufReader::with_capacity(HEADER + MAX_DATAGRAM, inner),
            datagram: vec![0; MAX_DATAGRAM].into_boxed_slice(),
        }
    }

    /// The next datagram, or `None` once the peer is done.
    ///
    /// Not cancel-safe: dropping this future mid-frame loses the bytes it has
    /// consumed and desynchronises the stream.
    pub async fn next(&mut self) -> io::Result<Option<&[u8]>> {
        let mut header = [0; HEADER];
        let read = self.inner.read(&mut header).await?;
        if read == 0 {
            return Ok(None);
        }
        self.inner.read_exact(&mut header[read..]).await?;

        let len = usize::from(u16::from_le_bytes(header));
        if len == 0 {
            return Ok(None);
        }
        let datagram = &mut self.datagram[..len];
        self.inner.read_exact(datagram).await?;
        Ok(Some(datagram))
    }
}

/// Writes frames onto a stream.
pub struct FrameWriter<W> {
    inner: W,
    frame: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            frame: Vec::with_capacity(HEADER + MAX_DATAGRAM),
        }
    }

    /// Sends one datagram as a single write, so it leaves as one TCP segment
    /// where it fits.
    ///
    /// An empty datagram is dropped, because an empty frame means "done" on
    /// the wire. Not cancel-safe: dropping this future can leave a partial
    /// frame on the stream.
    pub async fn send(&mut self, datagram: &[u8]) -> io::Result<()> {
        let len = u16::try_from(datagram.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "datagram too large for a frame")
        })?;
        if len == 0 {
            return Ok(());
        }
        self.frame.clear();
        self.frame.extend_from_slice(&len.to_le_bytes());
        self.frame.extend_from_slice(datagram);
        self.inner.write_all(&self.frame).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn written(datagrams: &[&[u8]]) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut writer = FrameWriter::new(&mut wire);
        for datagram in datagrams {
            writer.send(datagram).await.unwrap();
        }
        wire
    }

    #[tokio::test]
    async fn frame_is_le_length_then_payload() {
        let wire = written(&[b"abc", &[7; 300]]).await;

        assert_eq!(&wire[..5], &[3, 0, b'a', b'b', b'c']);
        assert_eq!(&wire[5..7], &[0x2c, 0x01]);
        assert_eq!(&wire[7..], &[7; 300]);
    }

    #[tokio::test]
    async fn round_trips_datagrams_in_order() {
        let largest = vec![0xab; MAX_DATAGRAM];
        let wire = written(&[b"first", &largest, b"x"]).await;

        let mut reader = FrameReader::new(wire.as_slice());
        assert_eq!(reader.next().await.unwrap(), Some(&b"first"[..]));
        assert_eq!(reader.next().await.unwrap(), Some(largest.as_slice()));
        assert_eq!(reader.next().await.unwrap(), Some(&b"x"[..]));
        assert_eq!(reader.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn reassembles_frames_delivered_a_byte_at_a_time() {
        let (near, far) = duplex(1);
        let sender = tokio::spawn(async move {
            let mut writer = FrameWriter::new(near);
            writer.send(b"hello").await.unwrap();
            writer.send(b"world!").await.unwrap();
        });

        let mut reader = FrameReader::new(far);
        assert_eq!(reader.next().await.unwrap(), Some(&b"hello"[..]));
        assert_eq!(reader.next().await.unwrap(), Some(&b"world!"[..]));
        assert_eq!(reader.next().await.unwrap(), None);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn empty_datagram_is_not_sent() {
        assert!(written(&[b""]).await.is_empty());
    }

    #[tokio::test]
    async fn oversized_datagram_is_rejected() {
        let mut wire = Vec::new();
        let err = FrameWriter::new(&mut wire)
            .send(&vec![0; MAX_DATAGRAM + 1])
            .await
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn zero_length_frame_ends_the_stream() {
        let wire = [0, 0, 1, 0, b'x'];
        assert_eq!(FrameReader::new(&wire[..]).next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn truncated_header_is_an_error() {
        let wire = [5];
        let err = FrameReader::new(&wire[..]).next().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn truncated_payload_is_an_error() {
        let wire = [5, 0, b'a', b'b'];
        let err = FrameReader::new(&wire[..]).next().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
