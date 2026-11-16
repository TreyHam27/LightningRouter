//! `sudo relay --system`: the privileged phase, then the stack on a
//! multi-thread runtime until the stack stops or the process is killed.
//!
//! There is no cleanup code. Ctrl-C (the default SIGINT action), `kill -9`
//! and a panic (`panic = "abort"`) all end the process, and the teardown is
//! the utun fd and the SCDynamicStore session closing with it (D8).

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{setup, stack};
use crate::device::Cache;

/// Never returns: exits 1 if setup fails or the stack stops.
pub fn run(upstream: SocketAddr) -> ! {
    // No runtime or thread may exist yet: this is still root.
    let prepared = match setup::prepare() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("relay: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "System mode on {} (service {}, fd limit {}); DNS forwards to {upstream}. Ctrl-C restores the network.",
        prepared.interface, prepared.service_id, prepared.fd_limit,
    );
    let setup::Prepared { device, store, .. } = prepared;
    // stack.rs needs multi-thread: dropping an ipstack TCP stream calls
    // block_in_place.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("relay: could not start the runtime: {e}");
            std::process::exit(1);
        }
    };
    let error = runtime.block_on(async move {
        let device = match tun_rs::AsyncDevice::new(device) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let cache = Cache::new();
        tokio::spawn(cache.clone().watch());
        let options = stack::Options {
            upstream,
            packet_information: false,
            mtu: setup::MTU,
        };
        stack::run(Tun(device), cache, options).await
    });
    eprintln!("relay: the stack stopped: {error}");
    drop(store);
    std::process::exit(1);
}

/// One IP packet per read and per write, as `stack::run` expects.
struct Tun(tun_rs::AsyncDevice);

impl AsyncRead for Tun {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let n = std::task::ready!(self.0.poll_recv(cx, buf.initialize_unfilled()))?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Tun {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.poll_send(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
