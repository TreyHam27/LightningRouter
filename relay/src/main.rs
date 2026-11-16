// Jackson Coxson

//! The Mac end of the link. Modes are exclusive; see `usage`.

mod device;
mod flow;
mod socks;
mod system;
mod wireguard;

use std::net::SocketAddr;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        // Before any runtime exists: --system starts as root (spec D8).
        ["--system"] => system::mode::run(system::stack::DEFAULT_UPSTREAM),
        ["--system", "--dns-upstream", addr] => system::mode::run(upstream(addr)),
        [] => block_on(wireguard::run()),
        ["--socks"] => block_on(socks_mode(socks::DEFAULT_ADDR)),
        ["--socks", addr] => block_on(socks_mode(addr)),
        _ => fail(usage()),
    }
}

fn block_on(mode: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(mode)
}

async fn socks_mode(addr: &str) {
    let cache = device::Cache::new();
    tokio::spawn(cache.clone().watch());
    socks::run(addr, cache).await;
}

/// `IP:PORT`, or a bare IP for port 53.
fn upstream(addr: &str) -> SocketAddr {
    addr.parse()
        .or_else(|_| addr.parse().map(|ip| SocketAddr::new(ip, 53)))
        .unwrap_or_else(|_| fail(&format!("--dns-upstream: not an IP or IP:PORT: {addr}")))
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}

fn usage() -> &'static str {
    "usage: relay                 WireGuard mode: UDP 3400 to the app over usbmuxd
       relay --socks [ADDR]  SOCKS5 proxy through the app (default 127.0.0.1:1080)
       sudo relay --system [--dns-upstream IP[:PORT]]
                             route the whole Mac through the app (default upstream 1.1.1.1)"
}
