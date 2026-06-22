use std::net::{Ipv4Addr, UdpSocket};

use simple_dns::Packet;
use smoltcp::wire::IpEndpoint;
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::ProxyConfig;
use crate::stack::StackCommand;
use crate::AllowedIps;

/// Handle a DNS query from the guest.
///
/// Resolves the query on the host and sends the response back via the stack.
/// When the domain allowlist is active, resolved IPs are added to the
/// allowed set so the proxy can enforce IP-level filtering.
pub async fn handle_dns_query(
    src: IpEndpoint,
    payload: Vec<u8>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    config: &ProxyConfig,
    allowed_ips: &AllowedIps,
) {
    let response = match resolve_query(&payload, config, allowed_ips).await {
        Ok(resp) => resp,
        Err(e) => {
            debug!("DNS resolution failed: {e}");
            return;
        }
    };

    let _ = cmd_tx.send(StackCommand::DnsResponse {
        dst: src,
        payload: response,
    });
}

async fn resolve_query(
    query_bytes: &[u8],
    config: &ProxyConfig,
    allowed_ips: &AllowedIps,
) -> anyhow::Result<Vec<u8>> {
    let query = Packet::parse(query_bytes)?;

    let question = query
        .questions
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty DNS query"))?;

    let qname = question.qname.to_string();
    let domain = qname.trim_end_matches('.');

    // The VM network stack is IPv4-only (smoltcp proto-ipv4).
    // Return empty NOERROR for AAAA queries — musl's getaddrinfo treats
    // REFUSED as a hard failure (EAI_AGAIN), but empty NOERROR means
    // "no AAAA records" and falls back to A records gracefully.
    let qtype = question.qtype;
    if qtype == simple_dns::QTYPE::TYPE(simple_dns::TYPE::AAAA) {
        debug!("DNS AAAA empty (IPv4-only): {domain}");
        return build_empty_response(query_bytes);
    }

    // Resolve host.dome.internal to the gateway IP so the guest can
    // reach exposed host ports without knowing the raw IP.
    if domain == "host.dome.internal" {
        debug!("DNS host.dome.internal -> 10.0.0.1");
        return build_a_response(query_bytes, Ipv4Addr::new(10, 0, 0, 1));
    }

    debug!("DNS query: {domain}");

    if !config.is_domain_allowed(domain) {
        debug!("DNS blocked: {domain}");
        return build_refused_response(query_bytes);
    }

    // Resolve on the host by forwarding to system resolver
    let response_bytes = tokio::task::spawn_blocking({
        let query = query_bytes.to_vec();
        move || forward_to_system_resolver(&query)
    })
    .await??;

    // Pin resolved IPs so the proxy allows direct connections to them.
    if config.network.has_allowlist() {
        pin_resolved_ips(&response_bytes, allowed_ips);
    }

    Ok(response_bytes)
}

/// Extract A record IPs from a DNS response and add them to the allowed set.
fn pin_resolved_ips(response: &[u8], allowed_ips: &AllowedIps) {
    let packet = match Packet::parse(response) {
        Ok(p) => p,
        Err(_) => return,
    };

    let mut ips = Vec::new();
    for answer in &packet.answers {
        if let simple_dns::rdata::RData::A(a) = &answer.rdata {
            ips.push(Ipv4Addr::from(a.address));
        }
    }

    if !ips.is_empty() {
        let mut cache = allowed_ips.write().expect("allowed_ips lock poisoned");
        for ip in &ips {
            debug!("DNS pin: {ip}");
            cache.put(*ip, ());
        }
    }
}

/// Forward a raw DNS query to the system resolver and return the raw response.
fn forward_to_system_resolver(query: &[u8]) -> anyhow::Result<Vec<u8>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    sock.send_to(query, "8.8.8.8:53")?;
    let mut buf = [0u8; 4096];
    let n = sock.recv(&mut buf)?;
    Ok(buf[..n].to_vec())
}

/// Build a DNS response with the given RCODE and no answer records.
fn build_response_with_rcode(query_bytes: &[u8], rcode: u8) -> anyhow::Result<Vec<u8>> {
    let mut response = query_bytes.to_vec();
    if response.len() < 12 {
        return Err(anyhow::anyhow!("query too short"));
    }
    // Set QR=1 (response), keep opcode, set RCODE
    response[2] |= 0x80;
    response[3] = (response[3] & 0xF0) | (rcode & 0x0F);
    Ok(response)
}

fn build_empty_response(query_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    build_response_with_rcode(query_bytes, 0)
}

/// Build a DNS A-record response pointing to the given IPv4 address.
fn build_a_response(query_bytes: &[u8], addr: Ipv4Addr) -> anyhow::Result<Vec<u8>> {
    use simple_dns::{rdata, ResourceRecord, CLASS};

    let query = Packet::parse(query_bytes)?;
    let qname = query
        .questions
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty DNS query"))?
        .qname
        .clone();

    let mut reply = query.into_reply();
    reply.answers.push(ResourceRecord::new(
        qname,
        CLASS::IN,
        60,
        rdata::RData::A(rdata::A::from(addr)),
    ));

    Ok(reply.build_bytes_vec()?)
}

fn build_refused_response(query_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    build_response_with_rcode(query_bytes, 5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lru::LruCache;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, RwLock};

    fn new_allowed(cap: usize) -> AllowedIps {
        Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(cap).unwrap())))
    }

    /// Build a minimal DNS A query for testing.
    fn build_a_query(domain: &str) -> Vec<u8> {
        use simple_dns::{Name, Packet, Question, QCLASS, QTYPE};
        let mut packet = Packet::new_query(1);
        packet.questions.push(Question::new(
            Name::new_unchecked(domain),
            QTYPE::TYPE(simple_dns::TYPE::A),
            QCLASS::CLASS(simple_dns::CLASS::IN),
            false,
        ));
        packet.build_bytes_vec().unwrap()
    }

    #[test]
    fn pin_resolved_ips_extracts_a_records() {
        let query = build_a_query("example.com");
        let response = build_a_response(&query, Ipv4Addr::new(93, 184, 216, 34)).unwrap();

        let allowed = new_allowed(16);
        pin_resolved_ips(&response, &allowed);

        let cache = allowed.read().unwrap();
        assert!(cache.contains(&Ipv4Addr::new(93, 184, 216, 34)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn pin_resolved_ips_ignores_invalid_response() {
        let allowed = new_allowed(16);
        pin_resolved_ips(b"not a dns packet", &allowed);

        assert!(allowed.read().unwrap().is_empty());
    }

    #[test]
    fn pin_resolved_ips_ignores_empty_response() {
        let query = build_a_query("example.com");
        let response = build_empty_response(&query).unwrap();

        let allowed = new_allowed(16);
        pin_resolved_ips(&response, &allowed);

        assert!(allowed.read().unwrap().is_empty());
    }

    #[test]
    fn pin_resolved_ips_evicts_oldest_when_full() {
        let allowed = new_allowed(2);
        let q = build_a_query("example.com");

        let r1 = build_a_response(&q, Ipv4Addr::new(1, 1, 1, 1)).unwrap();
        let r2 = build_a_response(&q, Ipv4Addr::new(2, 2, 2, 2)).unwrap();
        let r3 = build_a_response(&q, Ipv4Addr::new(3, 3, 3, 3)).unwrap();

        pin_resolved_ips(&r1, &allowed);
        pin_resolved_ips(&r2, &allowed);
        pin_resolved_ips(&r3, &allowed);

        let cache = allowed.read().unwrap();
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains(&Ipv4Addr::new(1, 1, 1, 1)));
        assert!(cache.contains(&Ipv4Addr::new(2, 2, 2, 2)));
        assert!(cache.contains(&Ipv4Addr::new(3, 3, 3, 3)));
    }
}
