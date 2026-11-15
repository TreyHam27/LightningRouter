//! Step 7b spike (spec D8, M1/M2/M3/M7/M8): run `setup::prepare` and the
//! DNS-only stub stack, with no phone involved. Needs sudo:
//!
//!     cargo build --release --example system_spike
//!     sudo target/release/examples/system_spike
//!
//! Ctrl-C (the default SIGINT action) and `kill -9` both end it with no
//! cleanup code at all; that is the point being tested.

#[path = "../src/system/setup.rs"]
mod setup;
#[path = "../src/system/stub.rs"]
mod stub;

fn main() {
    let prepared = match setup::prepare() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("system_spike: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "pid {} uid {} on {}, service {}, fd limit {}",
        std::process::id(),
        unsafe { libc::geteuid() },
        prepared.interface,
        prepared.service_id,
        prepared.fd_limit,
    );
    let setup::Prepared { device, store, .. } = prepared;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = runtime.block_on(async move {
        let device = tun_rs::AsyncDevice::new(device)?;
        stub::run(device).await
    });
    drop(store);
    eprintln!("system_spike: {result:?}");
}
