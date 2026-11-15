// Jackson Coxson

//! The Mac end of the link. Modes are exclusive; see `usage`.

mod device;
mod flow;
mod wireguard;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => wireguard::run().await,
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

fn usage() -> &'static str {
    "usage: relay    (WireGuard mode: UDP 3400 to the app over usbmuxd)"
}
