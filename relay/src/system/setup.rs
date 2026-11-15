//! Stage B's privileged phase (spec D8). Risky core.
//!
//! `prepare` runs as root, before the Tokio runtime starts. It raises the fd
//! limit, creates the utun, publishes this run's network service as
//! temporary SCDynamicStore values, and then drops to the sudo user. What it
//! returns, the utun and the store session, is the whole of the teardown:
//! when the process exits for any reason the kernel closes the utun fd
//! (the interface and every route through it go) and configd drops the
//! session's temporary values (IPMonitor restores the previous primary).
//!
//! Nothing here edits another service's keys, a `Setup:` key, or a route it
//! did not create, and nothing runs `networksetup`.
//!
//! This file uses no `crate::` items, so `examples/system_spike.rs` can
//! include it with `#[path]` for the step 7b manual runs.

use std::ffi::c_void;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use system_configuration::dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder};

/// The utun's local address and the service's `Router`.
pub const ROUTER_V4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
/// The DNS server the service publishes. The stack answers it.
pub const DNS_V4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);
/// 198.18.0.0/15: the utun's subnet and the fake-IP pool.
pub const PREFIX_V4: u8 = 15;
/// The utun's IPv6 address, `fd6c:7274::1` ("lr").
pub const ROUTER_V6: Ipv6Addr = Ipv6Addr::new(0xfd6c, 0x7274, 0, 0, 0, 0, 0, 1);
pub const PREFIX_V6: u8 = 64;
pub const MTU: u16 = 1500;
/// The relay's soft `RLIMIT_NOFILE` target (D10).
pub const FD_LIMIT: u64 = 8192;

/// What survives the privilege drop. Dropping it is the teardown.
pub struct Prepared {
    /// The utun. Wrap it with `tun_rs::AsyncDevice::new` inside the runtime.
    pub device: tun_rs::SyncDevice,
    /// `utunN`.
    pub interface: String,
    /// `LightningRouter-<pid>`.
    pub service_id: String,
    /// Holds the temporary keys alive. Never written to after the drop.
    pub store: SCDynamicStore,
    pub fd_limit: u64,
}

#[derive(Debug, PartialEq)]
pub enum SetupError {
    NotRoot,
    /// `SUDO_UID`/`SUDO_GID` missing, unparsable or zero.
    NoSudoUser(&'static str),
    /// A route into 198.18.0.0/15 already exists (Clash, Surge, a stale run).
    RangeInUse(String),
    RouteTable(String),
    Utun(String),
    Store(String),
    Drop(String),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetupError::NotRoot => write!(f, "--system needs root: run it with sudo"),
            SetupError::NoSudoUser(why) => write!(
                f,
                "refusing to keep running as root: {why}; start it with sudo from your own account"
            ),
            SetupError::RangeInUse(route) => write!(
                f,
                "198.18.0.0/15 is already routed ({route}); quit the other tool using it first"
            ),
            SetupError::RouteTable(e) => write!(f, "could not read the route table: {e}"),
            SetupError::Utun(e) => write!(f, "could not create the utun: {e}"),
            SetupError::Store(e) => write!(f, "could not publish the network service: {e}"),
            SetupError::Drop(e) => write!(f, "could not drop root: {e}"),
        }
    }
}

impl std::error::Error for SetupError {}

/// The privileged phase. Call it before any runtime or thread exists.
///
/// Checks run before anything changes, so every early error leaves the Mac
/// untouched. A failure after the utun exists returns early and the
/// partial state is dropped with the locals.
pub fn prepare() -> Result<Prepared, SetupError> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(SetupError::NotRoot);
    }
    let ids = sudo_ids(
        std::env::var("SUDO_UID").ok().as_deref(),
        std::env::var("SUDO_GID").ok().as_deref(),
    )?;
    let routes = std::process::Command::new("/usr/sbin/netstat")
        .args(["-rn", "-f", "inet"])
        .output()
        .map_err(|e| SetupError::RouteTable(e.to_string()))?;
    if !routes.status.success() {
        return Err(SetupError::RouteTable(format!(
            "netstat exited {}",
            routes.status
        )));
    }
    if let Some(route) = route_in_range(&String::from_utf8_lossy(&routes.stdout)) {
        return Err(SetupError::RangeInUse(route));
    }

    let fd_limit = raise_fd_limit();

    // associate_route (tun-rs's default on a utun) adds 198.18.0.0/15 and
    // fd6c:7274::/64 through the utun; both die with the interface.
    let device = tun_rs::DeviceBuilder::new()
        .ipv4(ROUTER_V4, PREFIX_V4, None)
        .ipv6(ROUTER_V6, PREFIX_V6)
        .mtu(MTU)
        .enable(true)
        .with(|b| {
            b.packet_information(false).associate_route(true);
        })
        .build_sync()
        .map_err(|e| SetupError::Utun(e.to_string()))?;
    let interface = device.name().map_err(|e| SetupError::Utun(e.to_string()))?;

    let service_id = service_id(std::process::id());
    let store = SCDynamicStoreBuilder::new("LightningRouter")
        .build()
        .ok_or_else(|| SetupError::Store("SCDynamicStoreCreate failed".into()))?;
    for (key, dict) in service_entries(&service_id, &interface) {
        if !add_temporary_value(&store, &key, &to_cf(&dict)) {
            // The keys added so far go with `store` when it drops here.
            return Err(SetupError::Store(format!("{key}: {}", sc_error())));
        }
    }

    drop_privileges(ids)?;
    Ok(Prepared {
        device,
        interface,
        service_id,
        store,
        fd_limit,
    })
}

/// `LightningRouter-<pid>`: new for every run, so a stale run's keys (if
/// configd ever kept them) never collide with this one's.
pub fn service_id(pid: u32) -> String {
    format!("LightningRouter-{pid}")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ids {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

/// The ids to drop to. Refuses rather than staying root: a missing or zero
/// `SUDO_UID` means root started us directly (or sudo from a root shell),
/// and there is no unprivileged user to become.
pub fn sudo_ids(uid: Option<&str>, gid: Option<&str>) -> Result<Ids, SetupError> {
    let uid: libc::uid_t = uid
        .ok_or(SetupError::NoSudoUser("SUDO_UID is not set"))?
        .parse()
        .map_err(|_| SetupError::NoSudoUser("SUDO_UID is not a number"))?;
    let gid: libc::gid_t = gid
        .ok_or(SetupError::NoSudoUser("SUDO_GID is not set"))?
        .parse()
        .map_err(|_| SetupError::NoSudoUser("SUDO_GID is not a number"))?;
    if uid == 0 {
        return Err(SetupError::NoSudoUser("SUDO_UID is 0"));
    }
    if gid == 0 {
        return Err(SetupError::NoSudoUser("SUDO_GID is 0"));
    }
    Ok(Ids { uid, gid })
}

/// Group first, then user (after `setuid` we could no longer change
/// groups), then prove it stuck.
fn drop_privileges(ids: Ids) -> Result<(), SetupError> {
    let os = |what: &str| SetupError::Drop(format!("{what}: {}", std::io::Error::last_os_error()));
    unsafe {
        if libc::setgroups(1, &ids.gid) != 0 {
            return Err(os("setgroups"));
        }
        if libc::setgid(ids.gid) != 0 {
            return Err(os("setgid"));
        }
        if libc::setuid(ids.uid) != 0 {
            return Err(os("setuid"));
        }
        if libc::setuid(0) == 0 || libc::seteuid(0) == 0 {
            return Err(SetupError::Drop("setuid(0) still succeeds".into()));
        }
        if libc::getuid() != ids.uid || libc::geteuid() != ids.uid || libc::getegid() != ids.gid {
            return Err(SetupError::Drop("ids did not change".into()));
        }
    }
    Ok(())
}

/// Soft `RLIMIT_NOFILE` to `min(hard, FD_LIMIT)`, never lowered. Returns
/// the soft limit in force afterwards.
fn raise_fd_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 256;
    }
    let target = limit.rlim_max.min(FD_LIMIT);
    if target > limit.rlim_cur {
        let raised = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return target;
        }
    }
    limit.rlim_cur
}

/// The first route in `netstat -rn` output whose destination lies inside
/// 198.18.0.0/15. Wider routes (`default`, `128.0/1`) don't count: they
/// are other tools' business and lose to our more specific /15.
pub fn route_in_range(table: &str) -> Option<String> {
    table
        .lines()
        .filter_map(|line| {
            let dest = line.split_whitespace().next()?;
            let (ip, prefix) = parse_destination(dest)?;
            let o = ip.octets();
            (prefix >= PREFIX_V4 && o[0] == 198 && (o[1] == 18 || o[1] == 19)).then(|| {
                line.split_whitespace()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
        .next()
}

/// netstat writes `198.18/15`, `198.18.0.5`, `198.18.0` (classful: three
/// octets means /24) or `198.18.0.5/32`. Headers and `default` give None.
fn parse_destination(dest: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = match dest.split_once('/') {
        Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
        None => (dest, None),
    };
    let parts: Vec<u8> = addr
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    octets[..parts.len()].copy_from_slice(&parts);
    let prefix = prefix.unwrap_or(8 * parts.len() as u8);
    (prefix <= 32).then_some((Ipv4Addr::from(octets), prefix))
}

/// A plist value, kept as plain Rust so the dictionaries test without CF.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i32),
    Strs(Vec<String>),
    Ints(Vec<i32>),
}

pub type Dict = Vec<(&'static str, Value)>;

/// The three temporary keys of this run's service, in publish order. DNS
/// goes last so the resolver never points at a service with no route.
pub fn service_entries(service_id: &str, interface: &str) -> Vec<(String, Dict)> {
    let key = |entity: &str| format!("State:/Network/Service/{service_id}/{entity}");
    vec![
        (key("IPv4"), ipv4_dict(interface)),
        (key("IPv6"), ipv6_dict(interface)),
        (key("DNS"), dns_dict()),
    ]
}

/// The key names are the values of `kSCPropNetIPv4*`, `kSCPropInterfaceName`
/// and `kSCPropNetOverridePrimary`.
pub fn ipv4_dict(interface: &str) -> Dict {
    vec![
        ("Addresses", Value::Strs(vec![ROUTER_V4.to_string()])),
        (
            "SubnetMasks",
            Value::Strs(vec![mask_v4(PREFIX_V4).to_string()]),
        ),
        ("Router", Value::Str(ROUTER_V4.to_string())),
        ("InterfaceName", Value::Str(interface.to_string())),
        ("OverridePrimary", Value::Int(1)),
    ]
}

/// IPv6 says `PrefixLength` (`kSCPropNetIPv6PrefixLength`) where IPv4 says
/// `SubnetMasks`.
pub fn ipv6_dict(interface: &str) -> Dict {
    vec![
        ("Addresses", Value::Strs(vec![ROUTER_V6.to_string()])),
        ("PrefixLength", Value::Ints(vec![PREFIX_V6 as i32])),
        ("Router", Value::Str(ROUTER_V6.to_string())),
        ("InterfaceName", Value::Str(interface.to_string())),
        ("OverridePrimary", Value::Int(1)),
    ]
}

pub fn dns_dict() -> Dict {
    vec![("ServerAddresses", Value::Strs(vec![DNS_V4.to_string()]))]
}

fn mask_v4(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix as u32).unwrap_or(0))
}

fn to_cf(dict: &Dict) -> CFDictionary<CFString, CFType> {
    let strings = |v: &[String]| {
        CFArray::from_CFTypes(&v.iter().map(|s| CFString::new(s)).collect::<Vec<_>>())
    };
    let pairs: Vec<(CFString, CFType)> = dict
        .iter()
        .map(|(k, v)| {
            let value = match v {
                Value::Str(s) => CFString::new(s).as_CFType(),
                Value::Int(n) => CFNumber::from(*n).as_CFType(),
                Value::Strs(v) => strings(v).as_CFType(),
                Value::Ints(v) => {
                    CFArray::from_CFTypes(&v.iter().map(|n| CFNumber::from(*n)).collect::<Vec<_>>())
                        .as_CFType()
                }
            };
            (CFString::from_static_string(k), value)
        })
        .collect();
    CFDictionary::from_CFType_pairs(&pairs)
}

/// `SCDynamicStoreAddTemporaryValue`: the value lives only as long as this
/// session. The safe wrapper has no such call (system-configuration 0.8).
/// Fails if the key already exists, which for a per-pid key means a stale
/// value from a previous process with our pid.
fn add_temporary_value(
    store: &SCDynamicStore,
    key: &str,
    value: &CFDictionary<CFString, CFType>,
) -> bool {
    let key = CFString::new(key);
    // SAFETY: all three refs are live CF objects owned by the caller for the
    // duration of the call; configd copies the value.
    unsafe {
        system_configuration_sys::dynamic_store::SCDynamicStoreAddTemporaryValue(
            store.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
            value.as_concrete_TypeRef() as *const c_void,
        ) != 0
    }
}

// Not bound by system-configuration-sys 0.6.
#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCError() -> std::ffi::c_int;
    fn SCErrorString(status: std::ffi::c_int) -> *const std::ffi::c_char;
}

/// The calling thread's last SystemConfiguration error, as text.
fn sc_error() -> String {
    // SAFETY: SCErrorString returns a static C string for any code.
    unsafe {
        let code = SCError();
        let msg = std::ffi::CStr::from_ptr(SCErrorString(code));
        format!("{} ({code})", msg.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
        &dict
            .iter()
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("no {key}"))
            .1
    }

    #[test]
    fn service_dictionaries_have_the_expected_keys() {
        let entries = service_entries("LightningRouter-42", "utun7");
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "State:/Network/Service/LightningRouter-42/IPv4",
                "State:/Network/Service/LightningRouter-42/IPv6",
                "State:/Network/Service/LightningRouter-42/DNS",
            ]
        );

        let v4 = &entries[0].1;
        assert_eq!(
            get(v4, "Addresses"),
            &Value::Strs(vec!["198.18.0.1".into()])
        );
        assert_eq!(
            get(v4, "SubnetMasks"),
            &Value::Strs(vec!["255.254.0.0".into()])
        );
        assert_eq!(get(v4, "Router"), &Value::Str("198.18.0.1".into()));
        assert_eq!(get(v4, "InterfaceName"), &Value::Str("utun7".into()));
        assert_eq!(get(v4, "OverridePrimary"), &Value::Int(1));

        let v6 = &entries[1].1;
        assert_eq!(
            get(v6, "Addresses"),
            &Value::Strs(vec!["fd6c:7274::1".into()])
        );
        assert_eq!(get(v6, "PrefixLength"), &Value::Ints(vec![64]));
        assert_eq!(get(v6, "Router"), &Value::Str("fd6c:7274::1".into()));
        assert_eq!(get(v6, "InterfaceName"), &Value::Str("utun7".into()));
        assert_eq!(get(v6, "OverridePrimary"), &Value::Int(1));

        assert_eq!(
            entries[2].1,
            vec![("ServerAddresses", Value::Strs(vec!["198.18.0.2".into()]))]
        );

        // Every dictionary converts to CF with one entry per key.
        for (_, dict) in &entries {
            assert_eq!(to_cf(dict).len(), dict.len());
        }
    }

    #[test]
    fn service_id_includes_pid() {
        assert_eq!(service_id(4321), "LightningRouter-4321");
        assert_ne!(service_id(1), service_id(2));
    }

    #[test]
    fn privilege_drop_needs_sudo_ids() {
        assert_eq!(
            sudo_ids(Some("501"), Some("20")),
            Ok(Ids { uid: 501, gid: 20 })
        );
        for (uid, gid) in [
            (None, Some("20")),
            (Some("501"), None),
            (Some("0"), Some("20")),
            (Some("501"), Some("0")),
            (Some(""), Some("20")),
            (Some("-1"), Some("20")),
            (Some("bob"), Some("20")),
        ] {
            assert!(
                matches!(sudo_ids(uid, gid), Err(SetupError::NoSudoUser(_))),
                "{uid:?} {gid:?} should refuse"
            );
        }
    }

    const TABLE: &str = "\
Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            192.168.1.1        UGScg                 en0
127                127.0.0.1          UCS                   lo0
127.0.0.1          127.0.0.1          UH                    lo0
128.0/1            utun3              USc                 utun3
169.254            link#11            UCS                   en0      !
192.168.1          link#11            UCS                   en0      !
198                10.0.0.1           UGSc                  en0
198.20/16          10.0.0.1           UGSc                  en0
224.0.0/4          link#11            UmCS                  en0      !
255.255.255.255/32 link#11            UCS                   en0      !
";

    #[test]
    fn refuses_to_start_when_198_18_is_already_routed() {
        assert_eq!(route_in_range(TABLE), None);
        for (line, want) in [
            (
                "198.18/15          utun4              USc                 utun4",
                "198.18/15 utun4 USc utun4",
            ),
            (
                "198.18.0/24        link#20            UCS                 utun4",
                "198.18.0/24 link#20 UCS utun4",
            ),
            (
                "198.19.3.4         utun4              UH                  utun4",
                "198.19.3.4 utun4 UH utun4",
            ),
            (
                "198.18.0.1/32      link#20            UCS                 utun4",
                "198.18.0.1/32 link#20 UCS utun4",
            ),
        ] {
            let table = format!("{TABLE}{line}\n");
            assert_eq!(route_in_range(&table).as_deref(), Some(want), "{line}");
        }
    }
}
