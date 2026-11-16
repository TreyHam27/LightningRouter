//! Step 6, D-A2 (the D1 gate): open N concurrent TCP flows through the
//! phone and report p50/p99 open latency and how many succeed.
//!
//!     cargo run --release --example flood [N] [host:port[,host:port...]|ping]
//!
//! Several comma-separated targets are used round robin. `FLOOD_GAP_MS=n`
//! starts the flows n ms apart instead of all at once.
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

/// "ping" measures only the usbmuxd connect plus the phone's accept, with
/// no outbound connection on the phone.
fn parse_target(target: &str) -> Request {
    if target == "ping" {
        return Request::ping();
    }
    let (host, port) = target.rsplit_once(':').expect("target is host:port or ping");
    let port: u16 = port.parse().expect("port");
    match host.parse() {
        Ok(ip) => Request::tcp(Addr::V4(ip), port),
        Err(_) => Request::tcp(Addr::Domain(host.into()), port),
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(256);
    let target = args.next().unwrap_or_else(|| "1.1.1.1:443".into());
    // "ping" measures only the usbmuxd connect plus the phone's accept, with
    // no outbound connection on the phone.
    let requests: Vec<Request> = target.split(',').map(parse_target).collect();
    let gap = Duration::from_millis(
        std::env::var("FLOOD_GAP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
    );

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
    for i in 0..n {
        if !gap.is_zero() && i > 0 {
            tokio::time::sleep(gap).await;
        }
        let cache = cache.clone();
        let request = requests[i % requests.len()].clone();
        tasks.push(tokio::spawn(async move {
            let t = Instant::now();
            let result = flow::open_flow(&cache, &request).await;
            (t.elapsed(), result)
        }));
    }
    let mut ok = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut held = Vec::new();
    // Successes per block of 32 flows, in start order, to show where they stop.
    let mut blocks = vec![0usize; n.div_ceil(32)];
    for (i, task) in tasks.into_iter().enumerate() {
        let (elapsed, result) = task.await.unwrap();
        match result {
            Ok(stream) => {
                blocks[i / 32] += 1;
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
    println!("successes per 32 in start order: {blocks:?}");
    // A failed usbmuxd connect marks the cache stale with its reason.
    if let device::State::Waiting(reason) = cache.state() {
        println!("last usbmuxd connect failure: {reason}");
    }
    drop(held);
}
