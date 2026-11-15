//! A stand-in for `stack.rs` for the step 7b spike: it answers DNS sent to
//! 198.18.0.2 and drops every other packet. That is enough to see that the
//! service is primary, that the resolver reaches the utun, and that
//! teardown restores the Mac. It is not fakedns: every A query gets the
//! first pool address, every other type an empty NOERROR.
//!
//! Like `setup.rs` it uses no `crate::` items, so the spike example can
//! include it with `#[path]`.

use std::net::Ipv4Addr;

/// The address every A query gets: the first pool address.
pub const ANSWER_V4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 3);
const DNS_V4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 2);

/// Reads packets until the device fails; replies to DNS, drops the rest.
pub async fn run(device: tun_rs::AsyncDevice) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65535];
    let (mut answered, mut dropped) = (0u64, 0u64);
    loop {
        let n = device.recv(&mut buf).await?;
        match answer(&buf[..n]) {
            Some(reply) => {
                device.send(&reply).await?;
                answered += 1;
                eprintln!("stub: answered DNS query {answered}");
            }
            None => {
                dropped += 1;
                if dropped.is_power_of_two() {
                    eprintln!("stub: dropped {dropped} non-DNS packets so far");
                }
            }
        }
    }
}

/// The reply packet for an IPv4 UDP DNS query to 198.18.0.2:53, or None.
pub fn answer(packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < 20 || packet[0] >> 4 != 4 || packet[9] != 17 {
        return None;
    }
    let ihl = usize::from(packet[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    if ihl < 20 || total < ihl + 8 || total > packet.len() {
        return None;
    }
    let src = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    let udp = &packet[ihl..total];
    let (sport, dport) = (
        u16::from_be_bytes([udp[0], udp[1]]),
        u16::from_be_bytes([udp[2], udp[3]]),
    );
    if dst != DNS_V4 || dport != 53 {
        return None;
    }
    let dns = dns_reply(&udp[8..])?;
    Some(ipv4_udp(dst, src, 53, sport, &dns))
}

/// One question in, the same question plus (for A/IN) one answer out.
fn dns_reply(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None; // too short, or already a response
    }
    if u16::from_be_bytes([query[4], query[5]]) != 1 {
        return None;
    }
    // Walk the QNAME labels (no compression in a question).
    let mut i = 12;
    loop {
        let len = usize::from(*query.get(i)?);
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        i += 1 + len;
    }
    let q_end = i + 4;
    let qtype = u16::from_be_bytes([*query.get(i)?, *query.get(i + 1)?]);
    let qclass = u16::from_be_bytes([*query.get(i + 2)?, *query.get(q_end - 1)?]);
    let is_a = qtype == 1 && qclass == 1;

    let mut out = Vec::with_capacity(q_end + 16);
    out.extend_from_slice(&query[0..2]); // ID
    // QR=1, opcode copied, AA=1, RD copied; RA=1, rcode 0.
    out.push(0x80 | (query[2] & 0x78) | 0x04 | (query[2] & 0x01));
    out.push(0x80);
    out.extend_from_slice(&[0, 1, 0, is_a as u8, 0, 0, 0, 0]);
    out.extend_from_slice(&query[12..q_end]);
    if is_a {
        // Name pointer to the question, A, IN, TTL 0, 4 bytes.
        out.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 0, 0, 4]);
        out.extend_from_slice(&ANSWER_V4.octets());
    }
    Some(out)
}

/// An IPv4 header with a correct checksum, then UDP with checksum 0
/// (allowed on IPv4).
fn ipv4_udp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len() as u16;
    let total = 20 + udp_len;
    let mut p = Vec::with_capacity(usize::from(total));
    p.extend_from_slice(&[0x45, 0]);
    p.extend_from_slice(&total.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0x40, 0, 64, 17, 0, 0]); // id, DF, TTL, UDP
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let sum = checksum(&p);
    p[10..12].copy_from_slice(&sum.to_be_bytes());
    p.extend_from_slice(&sport.to_be_bytes());
    p.extend_from_slice(&dport.to_be_bytes());
    p.extend_from_slice(&udp_len.to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);
    p
}

fn checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header
        .chunks(2)
        .map(|c| u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])))
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);

    fn query(qtype: u16) -> Vec<u8> {
        let mut q = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x07example\x03com\x00");
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }

    #[test]
    fn a_query_to_the_dns_address_gets_the_first_pool_address() {
        let packet = ipv4_udp(CLIENT, DNS_V4, 50000, 53, &query(1));
        let reply = answer(&packet).expect("a reply");
        assert_eq!(checksum(&reply[..20]), 0, "IPv4 header checksum");
        assert_eq!(&reply[12..16], &DNS_V4.octets());
        assert_eq!(&reply[16..20], &CLIENT.octets());
        assert_eq!(&reply[20..24], &[0, 53, 0xc3, 0x50]); // 53 -> 50000
        let dns = &reply[28..];
        assert_eq!(&dns[..2], &[0xab, 0xcd]);
        assert_eq!(dns[2] & 0x81, 0x81, "QR and RD");
        assert_eq!(&dns[6..8], &[0, 1], "one answer");
        assert_eq!(&dns[dns.len() - 4..], &ANSWER_V4.octets());
    }

    #[test]
    fn other_types_get_an_empty_noerror() {
        let reply = answer(&ipv4_udp(CLIENT, DNS_V4, 5353, 53, &query(28))).unwrap();
        let dns = &reply[28..];
        assert_eq!(dns[3] & 0x0f, 0);
        assert_eq!(&dns[6..8], &[0, 0]);
        assert_eq!(&dns[12..], &query(28)[12..]);
    }

    #[test]
    fn everything_else_is_dropped() {
        assert!(
            answer(&ipv4_udp(
                CLIENT,
                Ipv4Addr::new(1, 1, 1, 1),
                5353,
                53,
                &query(1)
            ))
            .is_none()
        );
        assert!(answer(&ipv4_udp(CLIENT, DNS_V4, 5353, 54, &query(1))).is_none());
        assert!(answer(&ipv4_udp(CLIENT, DNS_V4, 5353, 53, &query(1)[..14])).is_none());
        assert!(answer(&[0x60; 40]).is_none());
    }
}
