//! The pure DNS responder for whole-system mode (spec D4).
//!
//! `FakeDns::respond` takes a query's bytes and returns what to do with it.
//! It does no I/O: the stack glue sends `Answer` bytes back, tunnels
//! `Forward` queries to the upstream resolver, and ignores `Drop`.
//! It also owns the fake-IP pool, which maps names to addresses in
//! 198.18.0.3 to 198.19.255.254 and back.
#![allow(dead_code)] // wired up by `system/stack.rs`

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;

/// The utun's own address and the service `Router`.
pub const ROUTER_ADDR: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
/// The DNS server address handed to the system.
pub const DNS_ADDR: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);
/// Answers carry this TTL, in seconds.
pub const TTL: u32 = 30;
/// Live entries the pool holds before it recycles.
pub const POOL_CAPACITY: usize = 65_536;

const BASE: u32 = u32::from_be_bytes([198, 18, 0, 0]);
/// Pool offsets from `BASE`: 3 (198.18.0.3) up to 198.19.255.254.
const FIRST: u32 = 3;
const LAST: u32 = 2 * 65_536 - 2;

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_AAAA: u16 = 28;
const TYPE_SVCB: u16 = 64;
const TYPE_HTTPS: u16 = 65;
const CLASS_IN: u16 = 1;

const RCODE_SERVFAIL: u8 = 2;
const RCODE_NXDOMAIN: u8 = 3;

/// What to do with a query.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Send these bytes back to the client.
    Answer(Vec<u8>),
    /// Send the query unchanged to the upstream resolver.
    Forward,
    /// Not a query we understand; send nothing.
    Drop,
}

struct Entry {
    name: String,
    /// Recency tick; the key of this entry in `Pool::by_age`.
    tick: u64,
    pins: u32,
}

/// Name to address and back, least recently used out first.
struct Pool {
    capacity: usize,
    /// Offsets below this have been handed out at least once.
    next_fresh: u32,
    clock: u64,
    by_name: HashMap<String, u32>,
    entries: HashMap<u32, Entry>,
    /// tick -> offset, oldest first. Pinned entries stay in here and are
    /// skipped when evicting.
    by_age: BTreeMap<u64, u32>,
}

impl Pool {
    fn new(capacity: usize) -> Pool {
        let range = (LAST - FIRST + 1) as usize;
        Pool {
            capacity: capacity.min(range),
            next_fresh: FIRST,
            clock: 0,
            by_name: HashMap::new(),
            entries: HashMap::new(),
            by_age: BTreeMap::new(),
        }
    }

    fn touch(&mut self, offset: u32) {
        self.clock += 1;
        let entry = self.entries.get_mut(&offset).expect("entry exists");
        self.by_age.remove(&entry.tick);
        entry.tick = self.clock;
        self.by_age.insert(self.clock, offset);
    }

    /// The address for `name` (already lowercase), or `None` when the pool is
    /// full and every entry is pinned.
    fn assign(&mut self, name: &str) -> Option<Ipv4Addr> {
        if let Some(&offset) = self.by_name.get(name) {
            self.touch(offset);
            return Some(offset_to_ip(offset));
        }
        let offset = if self.entries.len() < self.capacity {
            let offset = self.next_fresh;
            self.next_fresh += 1;
            offset
        } else {
            let victim = self
                .by_age
                .values()
                .copied()
                .find(|o| self.entries[o].pins == 0)?;
            let old = self.entries.remove(&victim).expect("victim exists");
            self.by_age.remove(&old.tick);
            self.by_name.remove(&old.name);
            victim
        };
        self.clock += 1;
        self.by_name.insert(name.to_owned(), offset);
        self.by_age.insert(self.clock, offset);
        self.entries.insert(
            offset,
            Entry { name: name.to_owned(), tick: self.clock, pins: 0 },
        );
        Some(offset_to_ip(offset))
    }
}

fn offset_to_ip(offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(BASE + offset)
}

fn ip_to_offset(ip: Ipv4Addr) -> Option<u32> {
    let offset = u32::from(ip).checked_sub(BASE)?;
    (FIRST..=LAST).contains(&offset).then_some(offset)
}

/// The responder and its pool.
pub struct FakeDns {
    pool: Pool,
}

impl Default for FakeDns {
    fn default() -> Self {
        FakeDns::new()
    }
}

impl FakeDns {
    pub fn new() -> FakeDns {
        FakeDns::with_capacity(POOL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> FakeDns {
        FakeDns { pool: Pool::new(capacity) }
    }

    /// The name a pool address stands for, if it is mapped. Does not count as
    /// a use.
    pub fn lookup_name(&self, ip: Ipv4Addr) -> Option<&str> {
        let offset = ip_to_offset(ip)?;
        self.pool.entries.get(&offset).map(|e| e.name.as_str())
    }

    /// Marks an open flow to `ip`: it is not recycled until unpinned as many
    /// times. Counts, so several flows to one address are fine. Returns false
    /// for an address that is not mapped.
    pub fn pin(&mut self, ip: Ipv4Addr) -> bool {
        let Some(entry) = ip_to_offset(ip).and_then(|o| self.pool.entries.get_mut(&o)) else {
            return false;
        };
        entry.pins += 1;
        true
    }

    pub fn unpin(&mut self, ip: Ipv4Addr) {
        if let Some(entry) = ip_to_offset(ip).and_then(|o| self.pool.entries.get_mut(&o)) {
            entry.pins = entry.pins.saturating_sub(1);
        }
    }

    /// Decides what to do with one DNS query (the bytes of a UDP payload, or
    /// of a TCP message without its length prefix).
    pub fn respond(&mut self, query: &[u8]) -> Outcome {
        let Some(q) = Question::parse(query) else {
            return Outcome::Drop;
        };
        if q.class != CLASS_IN {
            return Outcome::Forward;
        }
        match q.qtype {
            TYPE_A => match self.pool.assign(&q.name) {
                Some(ip) => Outcome::Answer(reply(query, &q, 0, Some(Rdata::A(ip)))),
                // Nothing recyclable: the client may retry later.
                None => Outcome::Answer(reply(query, &q, RCODE_SERVFAIL, None)),
            },
            TYPE_AAAA | TYPE_HTTPS | TYPE_SVCB => Outcome::Answer(reply(query, &q, 0, None)),
            TYPE_PTR => {
                let mapped = parse_ptr_name(&q.name).and_then(|ip| self.lookup_name(ip));
                match mapped {
                    Some(name) => {
                        let name = name.to_owned();
                        Outcome::Answer(reply(query, &q, 0, Some(Rdata::Ptr(&name))))
                    }
                    None => Outcome::Answer(reply(query, &q, RCODE_NXDOMAIN, None)),
                }
            }
            _ => Outcome::Forward,
        }
    }
}

/// The single question of a query.
struct Question {
    /// Lowercase, no trailing dot.
    name: String,
    qtype: u16,
    class: u16,
    /// Offset just past the question section.
    end: usize,
}

impl Question {
    fn parse(msg: &[u8]) -> Option<Question> {
        if msg.len() < 12 {
            return None;
        }
        let flags = u16::from_be_bytes([msg[2], msg[3]]);
        let is_response = flags & 0x8000 != 0;
        let opcode = (flags >> 11) & 0xf;
        let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
        if is_response || opcode != 0 || qdcount != 1 {
            return None;
        }
        let mut pos = 12;
        let mut name = String::new();
        loop {
            let len = *msg.get(pos)? as usize;
            pos += 1;
            if len == 0 {
                break;
            }
            // Compression pointers (0xc0) and reserved bits never appear in
            // a question; 63 is the longest label.
            if len > 63 {
                return None;
            }
            let label = msg.get(pos..pos + len)?;
            pos += len;
            if !name.is_empty() {
                name.push('.');
            }
            for &b in label {
                // Names go to the phone as ASCII (the flow codec); reject
                // anything that cannot be a host name.
                if !(b.is_ascii_graphic()) || b == b'.' {
                    return None;
                }
                name.push(b.to_ascii_lowercase() as char);
            }
            if name.len() > 253 {
                return None;
            }
        }
        // The root name is not something to resolve.
        if name.is_empty() {
            return None;
        }
        let tail = msg.get(pos..pos + 4)?;
        Some(Question {
            name,
            qtype: u16::from_be_bytes([tail[0], tail[1]]),
            class: u16::from_be_bytes([tail[2], tail[3]]),
            end: pos + 4,
        })
    }
}

enum Rdata<'a> {
    A(Ipv4Addr),
    Ptr(&'a str),
}

/// Builds the reply: the query's id, question and RD flag, then at most one
/// answer. Anything after the question (EDNS) is not echoed.
fn reply(query: &[u8], q: &Question, rcode: u8, answer: Option<Rdata>) -> Vec<u8> {
    let rd = query[2] & 0x01;
    let mut out = Vec::with_capacity(q.end + 32);
    out.extend_from_slice(&query[..2]);
    out.push(0x80 | rd); // QR=1, opcode 0, AA=0, TC=0, RD copied
    out.push(0x80 | rcode); // RA=1
    out.extend_from_slice(&[0, 1]);
    out.extend_from_slice(&[0, answer.is_some() as u8]);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&query[12..q.end]);
    if let Some(data) = answer {
        out.extend_from_slice(&[0xc0, 0x0c]); // the question's name
        out.extend_from_slice(&q.qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        match data {
            Rdata::A(ip) => {
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&ip.octets());
            }
            Rdata::Ptr(name) => {
                let wire = encode_name(name);
                out.extend_from_slice(&(wire.len() as u16).to_be_bytes());
                out.extend_from_slice(&wire);
            }
        }
    }
    out
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// `4.3.2.1.in-addr.arpa` to `1.2.3.4`.
fn parse_ptr_name(name: &str) -> Option<Ipv4Addr> {
    let rest = name.strip_suffix(".in-addr.arpa")?;
    let mut octets = [0u8; 4];
    let mut parts = rest.split('.');
    for slot in octets.iter_mut().rev() {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(Ipv4Addr::from(octets))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_with(id: u16, flags: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&flags.to_be_bytes());
        m.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        m.extend_from_slice(&encode_name(name));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        m
    }

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        query_with(0x1234, 0x0100, name, qtype)
    }

    fn answer(out: Outcome) -> Vec<u8> {
        match out {
            Outcome::Answer(b) => b,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    fn rcode(msg: &[u8]) -> u8 {
        msg[3] & 0xf
    }

    fn ancount(msg: &[u8]) -> u16 {
        u16::from_be_bytes([msg[6], msg[7]])
    }

    /// The address in the single A answer of a reply to `name`.
    fn a_addr(msg: &[u8], name: &str) -> Ipv4Addr {
        // header 12 + question (name + 4), then 2 name + type + class + ttl + rdlen
        let rdata = 12 + encode_name(name).len() + 4 + 12;
        Ipv4Addr::new(msg[rdata], msg[rdata + 1], msg[rdata + 2], msg[rdata + 3])
    }

    fn ask(dns: &mut FakeDns, name: &str) -> Ipv4Addr {
        a_addr(&answer(dns.respond(&query(name, TYPE_A))), name)
    }

    #[test]
    fn a_query_gets_a_pool_address_with_ttl_30() {
        let mut dns = FakeDns::new();
        let msg = answer(dns.respond(&query("example.com", TYPE_A)));
        assert_eq!(rcode(&msg), 0);
        assert_eq!(ancount(&msg), 1);
        let ip = a_addr(&msg, "example.com");
        assert_eq!(ip, Ipv4Addr::new(198, 18, 0, 3));
        let ttl_at = 12 + encode_name("example.com").len() + 4 + 6;
        assert_eq!(&msg[ttl_at..ttl_at + 4], &30u32.to_be_bytes());
        assert_eq!(dns.lookup_name(ip), Some("example.com"));
    }

    #[test]
    fn same_name_gets_the_same_address_case_insensitively() {
        let mut dns = FakeDns::new();
        let a = ask(&mut dns, "Example.COM");
        assert_eq!(ask(&mut dns, "example.com"), a);
        assert_ne!(ask(&mut dns, "other.example"), a);
        assert_eq!(dns.lookup_name(a), Some("example.com"));
    }

    #[test]
    fn aaaa_https_and_svcb_get_empty_noerror() {
        let mut dns = FakeDns::new();
        for qtype in [TYPE_AAAA, TYPE_HTTPS, TYPE_SVCB] {
            let msg = answer(dns.respond(&query("example.com", qtype)));
            assert_eq!(rcode(&msg), 0, "type {qtype}");
            assert_eq!(ancount(&msg), 0, "type {qtype}");
        }
        // Suppressed types must not use up pool addresses.
        assert!(dns.pool.entries.is_empty());
    }

    #[test]
    fn other_types_are_forwarded() {
        let mut dns = FakeDns::new();
        for qtype in [15, 16, 33] {
            // MX, TXT, SRV
            assert_eq!(dns.respond(&query("example.com", qtype)), Outcome::Forward);
        }
    }

    #[test]
    fn ptr_for_pool_address_returns_its_name_else_nxdomain() {
        let mut dns = FakeDns::new();
        let ip = ask(&mut dns, "example.com");
        let o = ip.octets();
        let ptr = format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0]);
        let msg = answer(dns.respond(&query(&ptr, TYPE_PTR)));
        assert_eq!(rcode(&msg), 0);
        assert_eq!(ancount(&msg), 1);
        let tail = &msg[msg.len() - encode_name("example.com").len()..];
        assert_eq!(tail, encode_name("example.com").as_slice());

        // Unmapped pool address, and an address outside the pool.
        for ptr in ["200.0.18.198.in-addr.arpa", "4.3.2.1.in-addr.arpa", "bogus.in-addr.arpa"] {
            let msg = answer(dns.respond(&query(ptr, TYPE_PTR)));
            assert_eq!(rcode(&msg), RCODE_NXDOMAIN, "{ptr}");
            assert_eq!(ancount(&msg), 0, "{ptr}");
        }
    }

    #[test]
    fn reply_keeps_id_question_and_rd_flag() {
        let mut dns = FakeDns::new();
        for (flags, rd) in [(0x0100u16, 1u8), (0x0000, 0)] {
            let q = query_with(0xbeef, flags, "Example.com", TYPE_A);
            let msg = answer(dns.respond(&q));
            assert_eq!(&msg[..2], &[0xbe, 0xef]);
            assert_eq!(msg[2] & 0x01, rd);
            assert_ne!(msg[2] & 0x80, 0, "QR set");
            // The question is echoed byte for byte, case included.
            assert_eq!(&msg[12..q.len()], &q[12..]);
            assert_eq!(&msg[4..6], &[0, 1]);
        }
    }

    #[test]
    fn malformed_query_is_dropped() {
        let mut dns = FakeDns::new();
        let good = query("example.com", TYPE_A);
        assert_eq!(dns.respond(&[]), Outcome::Drop);
        assert_eq!(dns.respond(&good[..11]), Outcome::Drop);
        assert_eq!(dns.respond(&good[..good.len() - 1]), Outcome::Drop, "truncated");
        // A response, a non-query opcode, and two questions.
        assert_eq!(dns.respond(&query_with(1, 0x8100, "a.b", TYPE_A)), Outcome::Drop);
        assert_eq!(dns.respond(&query_with(1, 0x2800, "a.b", TYPE_A)), Outcome::Drop);
        let mut two = good.clone();
        two[5] = 2;
        assert_eq!(dns.respond(&two), Outcome::Drop);
        // A compression pointer in the question.
        let mut ptr = good.clone();
        ptr[12] = 0xc0;
        assert_eq!(dns.respond(&ptr), Outcome::Drop);
        assert_eq!(dns.respond(&query(".", TYPE_A)), Outcome::Drop, "empty label");
        assert!(dns.pool.entries.is_empty());
    }

    #[test]
    fn pool_recycles_least_recently_used_when_full() {
        let mut dns = FakeDns::with_capacity(4);
        let ips: Vec<_> = ["a.x", "b.x", "c.x", "d.x"].iter().map(|n| ask(&mut dns, n)).collect();
        // Use `a` again, so `b` is now the oldest.
        assert_eq!(ask(&mut dns, "a.x"), ips[0]);
        let e = ask(&mut dns, "e.x");
        assert_eq!(e, ips[1], "b's address is recycled");
        assert_eq!(dns.lookup_name(e), Some("e.x"));
        assert_eq!(dns.lookup_name(ips[0]), Some("a.x"));
        // `b` is gone; asking again takes the next oldest (`c`).
        assert_eq!(ask(&mut dns, "b.x"), ips[2]);
        assert_eq!(dns.pool.entries.len(), 4);
    }

    #[test]
    fn pinned_address_is_not_recycled() {
        let mut dns = FakeDns::with_capacity(2);
        let a = ask(&mut dns, "a.x");
        let b = ask(&mut dns, "b.x");
        assert!(dns.pin(a));
        // `a` is oldest but pinned, so `b` goes.
        assert_eq!(ask(&mut dns, "c.x"), b);
        assert_eq!(dns.lookup_name(a), Some("a.x"));
        // Everything pinned: SERVFAIL, nothing evicted.
        let c = ask_ip(&mut dns, "c.x");
        assert!(dns.pin(c));
        let msg = answer(dns.respond(&query("d.x", TYPE_A)));
        assert_eq!(rcode(&msg), RCODE_SERVFAIL);
        assert_eq!(dns.lookup_name(a), Some("a.x"));
        // Pins count: two pins need two unpins.
        assert!(dns.pin(a));
        dns.unpin(a);
        dns.unpin(c);
        assert_eq!(ask(&mut dns, "d.x"), c);
        assert_eq!(dns.lookup_name(a), Some("a.x"));
        dns.unpin(a);
        assert_eq!(ask(&mut dns, "e.x"), a);
        assert!(!dns.pin(Ipv4Addr::new(1, 2, 3, 4)));
    }

    fn ask_ip(dns: &mut FakeDns, name: &str) -> Ipv4Addr {
        ask(dns, name)
    }

    #[test]
    fn dns_server_and_router_addresses_are_never_handed_out() {
        let mut dns = FakeDns::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..POOL_CAPACITY + 10 {
            let ip = ask(&mut dns, &format!("h{i}.test"));
            assert_ne!(ip, ROUTER_ADDR);
            assert_ne!(ip, DNS_ADDR);
            let o = ip.octets();
            assert!(o[0] == 198 && (o[1] == 18 || o[1] == 19), "{ip} is in the pool range");
            seen.insert(ip);
        }
        assert_eq!(seen.len(), POOL_CAPACITY);
        // The range itself excludes both, and the last host address.
        assert_eq!(offset_to_ip(FIRST), Ipv4Addr::new(198, 18, 0, 3));
        assert_eq!(offset_to_ip(LAST), Ipv4Addr::new(198, 19, 255, 254));
        assert_eq!(dns.lookup_name(ROUTER_ADDR), None);
        assert_eq!(dns.lookup_name(DNS_ADDR), None);
    }
}
