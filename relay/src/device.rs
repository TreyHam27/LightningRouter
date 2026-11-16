//! Finding the phone (D11): usbmuxd discovery, a cache of the result for the
//! flow hot path, and the `LR_DEVICE_ADDR` bypass.

// The cache and watcher are used by the flow modes in later steps.
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use idevice::{
    ReadWrite,
    usbmuxd::{Connection, UsbmuxdAddr},
};
use lightning_router_frame::{
    FLOW_PORT,
    flow::{Request, Status, read_status, write_request},
};
use tokio::{net::TcpStream, sync::Semaphore};

const USBMUXD_TAG: u32 = 69;
/// usbmuxd connects allowed in flight at once. macOS usbmuxd's accept queue
/// overflows on a burst (256 at once lost about a third with "device socket
/// io failed", step 6a), so later connects wait here instead of failing.
/// Only the connect handshake holds a permit, not the flow.
static USBMUXD_CONNECTS: Semaphore = Semaphore::const_new(32);
/// How often the watcher looks for the device again.
const POLL: Duration = Duration::from_secs(1);

/// A phone the relay can reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    Usbmuxd {
        device_id: u32,
        udid: String,
        usb: bool,
    },
    /// `LR_DEVICE_ADDR`: plain TCP to the flow server, e.g. the simulator.
    Addr(SocketAddr),
}

impl Device {
    fn describe(&self) -> String {
        match self {
            Device::Usbmuxd {
                udid, usb: true, ..
            } => format!("Connected to the app on {udid}"),
            Device::Usbmuxd {
                udid, usb: false, ..
            } => {
                format!("Connected to the app on {udid} over the network; USB is faster")
            }
            Device::Addr(addr) => format!("Using the app at {addr}"),
        }
    }
}

/// `LR_DEVICE_ADDR=host:port`, if set. A malformed value is an error.
pub fn addr_override() -> Result<Option<SocketAddr>, String> {
    match std::env::var("LR_DEVICE_ADDR") {
        Ok(v) => v
            .parse()
            .map(Some)
            .map_err(|e| format!("LR_DEVICE_ADDR {v:?} is not host:port ({e})")),
        Err(_) => Ok(None),
    }
}

/// Lists usbmuxd devices and picks one, preferring USB. Devices are listed
/// afresh each time because usbmuxd gives a replugged device a new ID. The
/// error is the reason to show the user.
pub async fn discover() -> Result<Device, String> {
    if let Some(addr) = addr_override()? {
        return Ok(Device::Addr(addr));
    }
    let devices = UsbmuxdAddr::default()
        .connect(USBMUXD_TAG)
        .await
        .map_err(|e| format!("usbmuxd is unreachable ({e})"))?
        .get_devices()
        .await
        .map_err(|e| format!("usbmuxd did not list devices ({e})"))?;
    let device = devices
        .iter()
        .find(|d| d.connection_type == Connection::Usb)
        .or(devices.first())
        .ok_or("no device is connected")?;
    Ok(Device::Usbmuxd {
        device_id: device.device_id,
        udid: device.udid.clone(),
        usb: device.connection_type == Connection::Usb,
    })
}

/// Connects to `port` on a device.
pub async fn connect(device: &Device, port: u16) -> Result<Box<dyn ReadWrite>, String> {
    match device {
        Device::Addr(addr) => {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| format!("cannot reach the app at {addr} ({e})"))?;
            Ok(Box::new(stream))
        }
        Device::Usbmuxd {
            device_id, udid, ..
        } => {
            let _permit = USBMUXD_CONNECTS.acquire().await.expect("never closed");
            UsbmuxdAddr::default()
            .connect(USBMUXD_TAG)
            .await
            .map_err(|e| format!("usbmuxd is unreachable ({e})"))?
            .connect_to_device(*device_id, port, "lightning-router")
            .await
            .map_err(|e| format!("enable the tunnel in the app on {udid} ({e})"))?
            .get_socket()
            .ok_or_else(|| format!("usbmuxd gave no connection to {udid}"))
        }
    }
}

/// WireGuard mode: discover, connect to `port` and say what happened.
pub async fn connect_usbmuxd(port: u16) -> Result<Box<dyn ReadWrite>, String> {
    let device = discover().await?;
    let link = connect(&device, port).await?;
    println!("{}", device.describe());
    Ok(link)
}

/// What the cache knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ready(Device),
    Waiting(String),
}

/// Decides when a state change is worth telling the user: once per change,
/// not once per poll. Pure, so it is tested without a device.
#[derive(Default)]
pub struct Reporter {
    last: Option<State>,
}

impl Reporter {
    pub fn note(&mut self, state: &State) -> Option<String> {
        if self.last.as_ref() == Some(state) {
            return None;
        }
        self.last = Some(state.clone());
        Some(match state {
            State::Ready(device) => device.describe(),
            State::Waiting(reason) => format!("Waiting: {reason}"),
        })
    }
}

/// The latest known state of the phone, shared with the flow hot path.
#[derive(Clone)]
pub struct Cache(Arc<Mutex<State>>);

impl Cache {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(State::Waiting(
            "looking for a device".into(),
        ))))
    }

    pub fn state(&self) -> State {
        self.0.lock().unwrap().clone()
    }

    pub fn set(&self, state: State) {
        *self.0.lock().unwrap() = state;
    }

    /// The device flows should use, or the reason there is none.
    pub fn device(&self) -> Result<Device, String> {
        match self.state() {
            State::Ready(device) => Ok(device),
            State::Waiting(reason) => Err(reason),
        }
    }

    /// A flow's connect failed: the device is no longer trusted until the
    /// watcher confirms it again. The flow itself is not retried.
    pub fn mark_stale(&self, reason: String) {
        self.set(State::Waiting(reason));
    }

    /// Discovers and pings once, updating the cache.
    pub async fn refresh(&self) {
        let state = match discover().await {
            Ok(device) => match ping(&device).await {
                Ok(()) => State::Ready(device),
                Err(reason) => State::Waiting(reason),
            },
            Err(reason) => State::Waiting(reason),
        };
        self.set(state);
    }

    /// Refreshes every second and reports changes, forever.
    pub async fn watch(self) {
        let mut reporter = Reporter::default();
        loop {
            self.refresh().await;
            if let Some(line) = reporter.note(&self.state()) {
                println!("{line}");
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

async fn ping(device: &Device) -> Result<(), String> {
    let mut link = connect(device, FLOW_PORT).await?;
    write_request(&mut link, &Request::ping())
        .await
        .map_err(|e| format!("the app did not take a ping ({e})"))?;
    match read_status(&mut link).await {
        Ok(Status::Ok) => Ok(()),
        Ok(status) => Err(format!("the app answered a ping with {status:?}")),
        Err(e) => Err(format!("the app did not answer a ping ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: u32) -> State {
        State::Ready(Device::Usbmuxd {
            device_id: id,
            udid: format!("U{id}"),
            usb: true,
        })
    }

    #[test]
    fn device_cache_reports_reason_once_per_change() {
        let mut reporter = Reporter::default();
        let waiting = |r: &str| State::Waiting(r.into());

        assert!(reporter.note(&waiting("no device is connected")).is_some());
        assert!(reporter.note(&waiting("no device is connected")).is_none());
        assert!(reporter.note(&waiting("enable the tunnel")).is_some());
        assert!(reporter.note(&device(1)).is_some());
        assert!(reporter.note(&device(1)).is_none());
        // Replugged: a new id is a change.
        assert!(reporter.note(&device(2)).is_some());
        // Back to a reason already given earlier is still a change.
        assert!(reporter.note(&waiting("no device is connected")).is_some());
    }

    #[test]
    fn stale_cache_has_no_device_until_refreshed() {
        let cache = Cache::new();
        cache.set(device(1));
        assert!(cache.device().is_ok());
        cache.mark_stale("gone".into());
        assert_eq!(cache.device(), Err("gone".to_string()));
    }
}
