//! Step 6, D-A2 (the D1 gate): open N concurrent TCP flows through the
//! phone and report p50/p99 open latency and how many succeed.
//!
//!     cargo run --release --example flood [N] [host:port|ping]
//!
//! Stop the relay first or leave it running; both talk to the same phone.
//! Flows are held open until every attempt has finished, then closed.

#![allow(dead_code)]

#[path = "../src/device.rs"]
mod device;
#[path = "../src/flow.rs"]
mod flow;

use std::time::{Duration, Instant};

use lightning_router_frame::flow::{Addr, Request};

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(256);
    let target = args.next().unwrap_or_else(|| "1.1.1.1:443".into());
    // "ping" measures only the usbmuxd connect plus the phone's accept, with
    // no outbound connection on the phone.
    let request = if target == "ping" {
        Request::ping()
    } else {
        let (host, port) = target.rsplit_once(':').expect("target is host:port or ping");
        let port: u16 = port.parse().expect("port");
        match host.parse() {
            Ok(ip) => Request::tcp(Addr::V4(ip), port),
            Err(_) => Request::tcp(Addr::Domain(host.into()), port),
        }
    };

    // 256 sockets would hit the default soft limit of 256 fds.
    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim);
        lim.rlim_cur = lim.rlim_max.min(10240);
        libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
    }

    let cache = device::Cache::new();
    cache.refresh().await;
    if let Err(reason) = cache.device() {
        eprintln!("flood: {reason}");
        std::process::exit(1);
    }

    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..n {
        let cache = cache.clone();
        let request = request.clone();
        tasks.push(tokio::spawn(async move {
            let t = Instant::now();
            let result = flow::open_flow(&cache, &request).await;
            (t.elapsed(), result)
        }));
    }
    let mut ok = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut held = Vec::new();
    for task in tasks {
        let (elapsed, result) = task.await.unwrap();
        match result {
            Ok(stream) => {
                ok.push(elapsed);
                held.push(stream);
            }
            Err(status) => failed.push(format!("{status:?}")),
        }
    }
    let total = start.elapsed();
    ok.sort();
    println!("target {target}, {n} concurrent flows, total {total:?}");
    println!("succeeded {}, failed {}", ok.len(), failed.len());
    if !failed.is_empty() {
        failed.sort();
        failed.dedup();
        println!("failure statuses: {failed:?}");
    }
    println!(
        "open latency p50 {:?}, p99 {:?}, max {:?}",
        percentile(&ok, 0.5),
        percentile(&ok, 0.99),
        ok.last().copied().unwrap_or_default()
    );
    drop(held);
}
