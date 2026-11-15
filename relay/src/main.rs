// Jackson Coxson

//! The Mac end of the link. Modes are exclusive; see `usage`.

mod device;
mod flow;
mod socks;
#[allow(dead_code)] // wired to --system in step 10
mod system;
mod wireguard;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => wireguard::run().await,
        [mode] if mode == "--socks" => socks_mode(socks::DEFAULT_ADDR).await,
        [mode, addr] if mode == "--socks" => socks_mode(addr).await,
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

async fn socks_mode(addr: &str) {
    let cache = device::Cache::new();
    tokio::spawn(cache.clone().watch());
    socks::run(addr, cache).await;
}

fn usage() -> &'static str {
    "usage: relay                 WireGuard mode: UDP 3400 to the app over usbmuxd\n       relay --socks [ADDR]  SOCKS5 proxy through the app (default 127.0.0.1:1080)"
}
