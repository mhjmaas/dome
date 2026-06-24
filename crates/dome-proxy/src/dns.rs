use std::net::{Ipv4Addr, UdpSocket};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use simple_dns::Packet;
use smoltcp::wire::IpEndpoint;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::ProxyConfig;
use crate::stack::StackCommand;
use crate::AllowedIps;

/// Shared handle to the host-side DNS answer cache.
pub type SharedDnsCache = Arc<DnsCache>;

struct CacheEntry {
    ips: Vec<Ipv4Addr>,
    expires_at: Instant,
}

/// Default cap on the number of distinct domains held in the DNS cache. Bounds
/// host memory even when a wildcard allowlist entry lets a hostile guest resolve
/// unlimited distinct subdomains. LRU eviction keeps actively-used hosts warm.
const DNS_CACHE_CAPACITY: usize = 4096;

/// A short-TTL, bounded cache of resolved A records, keyed by domain name.
///
/// Without it, a large `pnpm install` re-resolves the same handful of hosts
/// (e.g. `registry.npmjs.org`) hundreds of times, multiplying load on both the
/// upstream resolver and the small in-VM DNS socket buffers.
///
/// Bounded via LRU so a guest cannot grow it without limit (e.g. by resolving
/// endless subdomains of a wildcard-allowed domain). Time is passed in rather
/// than read internally so the eviction logic is deterministically testable.
pub struct DnsCache {
    entries: Mutex<LruCache<String, CacheEntry>>,
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsCache {
    pub fn new() -> Self {
        Self::with_capacity(DNS_CACHE_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("dns cache capacity must be non-zero");
        DnsCache {
            entries: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Return the cached IPs for `name` if present and not yet expired. A hit
    /// bumps the entry's recency; an expired entry is dropped so its slot frees.
    fn get(&self, name: &str, now: Instant) -> Option<Vec<Ipv4Addr>> {
        let mut entries = self.entries.lock().expect("dns cache lock poisoned");
        match entries.get(name) {
            Some(entry) if entry.expires_at > now => Some(entry.ips.clone()),
            Some(_) => {
                entries.pop(name);
                None
            }
            None => None,
        }
    }

    /// Cache `ips` for `name` for `ttl_secs`. A zero TTL is not stored (nothing
    /// to gain, and it keeps the "do not cache" DNS semantics intact).
    fn insert(&self, name: &str, ips: Vec<Ipv4Addr>, ttl_secs: u32, now: Instant) {
        if ttl_secs == 0 || ips.is_empty() {
            return;
        }
        let expires_at = now + Duration::from_secs(u64::from(ttl_secs));
        self.entries
            .lock()
            .expect("dns cache lock poisoned")
            .put(name.to_string(), CacheEntry { ips, expires_at });
    }

    /// Number of entries currently held (test-only visibility into bounding).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().expect("dns cache lock poisoned").len()
    }
}

/// Render a guest-supplied domain safe to log: DNS labels can carry arbitrary
/// bytes, so strip control characters (which could forge extra log lines) and
/// cap the length to keep one query from flooding the host log.
fn sanitize_for_log(name: &str) -> String {
    name.chars()
        .take(256)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

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
    cache: &SharedDnsCache,
) {
    let response = match resolve_query(&payload, config, allowed_ips, cache).await {
        Ok(resp) => resp,
        Err(e) => {
            // Don't drop the reply: an unanswered query makes the guest resolver
            // time out and surface a misleading EAI_AGAIN on whatever operation
            // was in flight. Send SERVFAIL so it fails fast and clearly instead.
            warn!("DNS resolution failed, returning SERVFAIL: {e}");
            match build_servfail_response(&payload) {
                Ok(resp) => resp,
                Err(_) => return,
            }
        }
    };

    let _ = cmd_tx.send(StackCommand::DnsResponse {
        dst: src,
        payload: response,
    });
}

/// TTL (seconds) advertised on answers served from the cache. Kept short so the
/// guest re-queries soon, which re-pins the IPs and re-checks the allowlist.
const CACHE_HIT_TTL: u32 = 30;
/// Upper bound (seconds) on how long an answer is held in the cache, regardless
/// of the upstream TTL, so the sandbox never pins a long-stale IP.
const MAX_CACHE_TTL: u32 = 300;

async fn resolve_query(
    query_bytes: &[u8],
    config: &ProxyConfig,
    allowed_ips: &AllowedIps,
    cache: &SharedDnsCache,
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
        // User-visible: this is the single most useful line for diagnosing a
        // too-strict allowlist — name the domain that needs adding. Sanitize
        // first: the name is guest-controlled and could otherwise forge logs.
        warn!(
            "DNS blocked (not in network.allow): {}",
            sanitize_for_log(domain)
        );
        return build_refused_response(query_bytes);
    }

    // Serve from cache when fresh — a large install re-resolves the same hosts
    // hundreds of times, and each uncached lookup competes for the small in-VM
    // DNS socket buffers.
    let now = Instant::now();
    if let Some(ips) = cache.get(domain, now) {
        debug!("DNS cache hit: {domain}");
        if config.network.has_allowlist() {
            pin_ips(&ips, allowed_ips);
        }
        return build_a_response_multi(query_bytes, &ips, CACHE_HIT_TTL);
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

    // Cache the answer (bounded TTL) for subsequent lookups of the same host.
    if let Some((ips, ttl)) = extract_a_records(&response_bytes) {
        cache.insert(domain, ips, ttl.min(MAX_CACHE_TTL), now);
    }

    Ok(response_bytes)
}

/// Extract the A-record IPs and the minimum TTL from a DNS response.
/// Returns None if the response has no A records.
fn extract_a_records(response: &[u8]) -> Option<(Vec<Ipv4Addr>, u32)> {
    let packet = Packet::parse(response).ok()?;
    let mut ips = Vec::new();
    let mut min_ttl = u32::MAX;
    for answer in &packet.answers {
        if let simple_dns::rdata::RData::A(a) = &answer.rdata {
            ips.push(Ipv4Addr::from(a.address));
            min_ttl = min_ttl.min(answer.ttl);
        }
    }
    if ips.is_empty() {
        None
    } else {
        Some((ips, min_ttl))
    }
}

/// Extract A record IPs from a DNS response and add them to the allowed set.
fn pin_resolved_ips(response: &[u8], allowed_ips: &AllowedIps) {
    if let Some((ips, _)) = extract_a_records(response) {
        pin_ips(&ips, allowed_ips);
    }
}

/// Add resolved IPs to the allowed set so the proxy permits direct connections
/// to them. Called for both freshly-resolved and cache-served answers.
fn pin_ips(ips: &[Ipv4Addr], allowed_ips: &AllowedIps) {
    if ips.is_empty() {
        return;
    }
    let mut cache = allowed_ips.write().expect("allowed_ips lock poisoned");
    for ip in ips {
        debug!("DNS pin: {ip}");
        cache.put(*ip, ());
    }
}

/// Parse `/etc/resolv.conf` contents into the list of IPv4 nameservers, in
/// file order. IPv6 nameservers are skipped because the guest network stack is
/// IPv4-only. Comments and non-`nameserver` directives are ignored.
fn parse_resolv_conf(contents: &str) -> Vec<Ipv4Addr> {
    let mut servers = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        if let Some(addr) = parts.next() {
            if let Ok(ip) = addr.parse::<Ipv4Addr>() {
                servers.push(ip);
            }
        }
    }
    servers
}

/// The nameservers to query, in priority order: those from the host's
/// `/etc/resolv.conf`, then a public fallback so resolution still works if the
/// host file is empty/unreadable.
fn resolver_addresses() -> Vec<Ipv4Addr> {
    const FALLBACK: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
    let mut servers = std::fs::read_to_string("/etc/resolv.conf")
        .map(|c| parse_resolv_conf(&c))
        .unwrap_or_default();
    if !servers.contains(&FALLBACK) {
        servers.push(FALLBACK);
    }
    servers
}

/// Forward a raw DNS query to the host's resolvers and return the raw response.
///
/// Tries each resolver in turn, with a couple of UDP attempts each, because a
/// single dropped UDP datagram (common when a large `pnpm install` fans out
/// hundreds of concurrent lookups) would otherwise stall the whole 5s timeout.
/// If a UDP answer comes back truncated (TC=1) it is re-fetched over TCP.
fn forward_to_system_resolver(query: &[u8]) -> anyhow::Result<Vec<u8>> {
    const UDP_ATTEMPTS: usize = 2;
    let mut last_err: Option<anyhow::Error> = None;

    for server in resolver_addresses() {
        for _ in 0..UDP_ATTEMPTS {
            match query_udp(query, server) {
                Ok(resp) if is_truncated(&resp) => {
                    // Truncated UDP answer — retry the whole thing over TCP.
                    match query_tcp(query, server) {
                        Ok(tcp_resp) => return Ok(tcp_resp),
                        Err(e) => last_err = Some(e),
                    }
                }
                Ok(resp) => return Ok(resp),
                Err(e) => last_err = Some(e),
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no resolvers available")))
}

/// Returns true if a DNS response has the TC (truncation) flag set.
fn is_truncated(response: &[u8]) -> bool {
    // Header byte 2, bit 1 (0x02) is the TC flag.
    response.len() >= 3 && response[2] & 0x02 != 0
}

/// Send a single UDP query to `server:53` and return the raw response.
fn query_udp(query: &[u8], server: Ipv4Addr) -> anyhow::Result<Vec<u8>> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    sock.send_to(query, (server, 53))?;
    let mut buf = [0u8; 4096];
    let n = sock.recv(&mut buf)?;
    Ok(buf[..n].to_vec())
}

/// Send a query to `server:53` over TCP (RFC 1035 §4.2.2: 2-byte length prefix)
/// and return the raw DNS message (length prefix stripped).
fn query_tcp(query: &[u8], server: Ipv4Addr) -> anyhow::Result<Vec<u8>> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from((server, 53)),
        std::time::Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;

    let len = u16::try_from(query.len())?.to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(query)?;

    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf)?;
    let resp_len = u16::from_be_bytes(len_buf) as usize;
    let mut resp = vec![0u8; resp_len];
    stream.read_exact(&mut resp)?;
    Ok(resp)
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

/// Build a NOERROR A-record response listing all `addrs`, echoing the query's
/// id and question. Used to serve answers straight from the DNS cache.
fn build_a_response_multi(
    query_bytes: &[u8],
    addrs: &[Ipv4Addr],
    ttl: u32,
) -> anyhow::Result<Vec<u8>> {
    use simple_dns::{rdata, ResourceRecord, CLASS};

    let query = Packet::parse(query_bytes)?;
    let qname = query
        .questions
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty DNS query"))?
        .qname
        .clone();

    let mut reply = query.into_reply();
    for addr in addrs {
        reply.answers.push(ResourceRecord::new(
            qname.clone(),
            CLASS::IN,
            ttl,
            rdata::RData::A(rdata::A::from(*addr)),
        ));
    }

    Ok(reply.build_bytes_vec()?)
}

fn build_refused_response(query_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    build_response_with_rcode(query_bytes, 5)
}

/// Build a SERVFAIL (RCODE 2) response. Sent when upstream resolution fails so
/// the guest resolver gets an immediate, well-formed failure instead of timing
/// out into a misleading `EAI_AGAIN` on whatever operation was in flight.
fn build_servfail_response(query_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    build_response_with_rcode(query_bytes, 2)
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
    fn parse_resolv_conf_extracts_ipv4_nameservers_in_order() {
        let contents = "\
# a comment
nameserver 1.1.1.1
search example.com
nameserver 8.8.4.4
options edns0
nameserver 2001:4860:4860::8888
  nameserver   9.9.9.9
";
        let servers = parse_resolv_conf(contents);
        // IPv4 nameservers, in file order; IPv6 skipped (stack is IPv4-only);
        // leading/trailing whitespace tolerated.
        assert_eq!(
            servers,
            vec![
                Ipv4Addr::new(1, 1, 1, 1),
                Ipv4Addr::new(8, 8, 4, 4),
                Ipv4Addr::new(9, 9, 9, 9),
            ]
        );
    }

    #[test]
    fn extract_a_records_returns_ips_and_min_ttl() {
        use simple_dns::{rdata, Name, ResourceRecord, CLASS};
        let query = build_a_query("registry.npmjs.org");
        let mut reply = Packet::parse(&query).unwrap().into_reply();
        reply.answers.push(ResourceRecord::new(
            Name::new_unchecked("registry.npmjs.org"),
            CLASS::IN,
            300,
            rdata::RData::A(rdata::A::from(Ipv4Addr::new(1, 1, 1, 1))),
        ));
        reply.answers.push(ResourceRecord::new(
            Name::new_unchecked("registry.npmjs.org"),
            CLASS::IN,
            60,
            rdata::RData::A(rdata::A::from(Ipv4Addr::new(2, 2, 2, 2))),
        ));
        let bytes = reply.build_bytes_vec().unwrap();

        let (ips, ttl) = extract_a_records(&bytes).unwrap();
        assert_eq!(
            ips,
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(2, 2, 2, 2)]
        );
        // Min TTL across the records, so the cache never serves a record past
        // its individual lifetime.
        assert_eq!(ttl, 60);
    }

    #[test]
    fn extract_a_records_none_when_no_a_records() {
        let query = build_a_query("example.com");
        let empty = build_empty_response(&query).unwrap();
        assert!(extract_a_records(&empty).is_none());
    }

    #[test]
    fn build_a_response_multi_echoes_query_and_lists_all_ips() {
        let query = build_a_query("registry.npmjs.org");
        let ips = vec![
            Ipv4Addr::new(104, 16, 0, 1),
            Ipv4Addr::new(104, 16, 0, 2),
            Ipv4Addr::new(104, 16, 0, 3),
        ];
        let resp = build_a_response_multi(&query, &ips, 42).unwrap();

        let packet = Packet::parse(&resp).unwrap();
        assert_eq!(packet.rcode(), simple_dns::RCODE::NoError);
        // Same id as the query so the guest resolver matches the answer.
        assert_eq!(&resp[0..2], &query[0..2]);

        let mut got: Vec<Ipv4Addr> = packet
            .answers
            .iter()
            .filter_map(|a| match &a.rdata {
                simple_dns::rdata::RData::A(x) => Some(Ipv4Addr::from(x.address)),
                _ => None,
            })
            .collect();
        got.sort();
        let mut want = ips.clone();
        want.sort();
        assert_eq!(got, want);
        assert!(packet.answers.iter().all(|a| a.ttl == 42));
    }

    #[test]
    fn dns_cache_miss_on_empty() {
        let cache = DnsCache::new();
        let now = std::time::Instant::now();
        assert!(cache.get("registry.npmjs.org", now).is_none());
    }

    #[test]
    fn dns_cache_hit_before_ttl_expires() {
        let cache = DnsCache::new();
        let now = std::time::Instant::now();
        let ips = vec![Ipv4Addr::new(104, 16, 0, 1), Ipv4Addr::new(104, 16, 0, 2)];
        cache.insert("registry.npmjs.org", ips.clone(), 60, now);

        // Still fresh one second later.
        let later = now + std::time::Duration::from_secs(1);
        assert_eq!(cache.get("registry.npmjs.org", later), Some(ips));
    }

    #[test]
    fn dns_cache_miss_after_ttl_expires() {
        let cache = DnsCache::new();
        let now = std::time::Instant::now();
        cache.insert(
            "registry.npmjs.org",
            vec![Ipv4Addr::new(1, 2, 3, 4)],
            30,
            now,
        );

        let after = now + std::time::Duration::from_secs(31);
        assert!(cache.get("registry.npmjs.org", after).is_none());
    }

    #[test]
    fn dns_cache_zero_ttl_is_never_stored() {
        let cache = DnsCache::new();
        let now = std::time::Instant::now();
        cache.insert(
            "registry.npmjs.org",
            vec![Ipv4Addr::new(1, 2, 3, 4)],
            0,
            now,
        );
        assert!(cache.get("registry.npmjs.org", now).is_none());
    }

    #[test]
    fn sanitize_for_log_strips_control_characters() {
        // A crafted DNS name with embedded CR/LF must not be able to forge a
        // second log line on the host.
        let forged = "evil.com\r\n2026-01-01 ERROR fake log line";
        let safe = sanitize_for_log(forged);
        assert!(!safe.contains('\n'));
        assert!(!safe.contains('\r'));
        assert!(safe.starts_with("evil.com"));
    }

    #[test]
    fn sanitize_for_log_truncates_overlong_input() {
        let long = "a".repeat(1000);
        assert!(sanitize_for_log(&long).len() <= 256);
    }

    #[test]
    fn dns_cache_is_bounded_by_capacity() {
        // A wildcard allowlist entry lets a hostile guest resolve unlimited
        // distinct subdomains; the cache must not grow without bound.
        let cache = DnsCache::with_capacity(2);
        let now = std::time::Instant::now();
        cache.insert("a.example.com", vec![Ipv4Addr::new(1, 1, 1, 1)], 60, now);
        cache.insert("b.example.com", vec![Ipv4Addr::new(2, 2, 2, 2)], 60, now);
        cache.insert("c.example.com", vec![Ipv4Addr::new(3, 3, 3, 3)], 60, now);

        assert_eq!(cache.len(), 2);
        // Oldest (least-recently-used) was evicted.
        assert!(cache.get("a.example.com", now).is_none());
        assert!(cache.get("b.example.com", now).is_some());
        assert!(cache.get("c.example.com", now).is_some());
    }

    #[test]
    fn dns_cache_get_keeps_active_entry_warm() {
        let cache = DnsCache::with_capacity(2);
        let now = std::time::Instant::now();
        cache.insert("a.example.com", vec![Ipv4Addr::new(1, 1, 1, 1)], 60, now);
        cache.insert("b.example.com", vec![Ipv4Addr::new(2, 2, 2, 2)], 60, now);

        // Touch "a" so it becomes most-recently-used, then overflow.
        assert!(cache.get("a.example.com", now).is_some());
        cache.insert("c.example.com", vec![Ipv4Addr::new(3, 3, 3, 3)], 60, now);

        // "b" was the least-recently-used and is evicted; "a" survives.
        assert!(cache.get("a.example.com", now).is_some());
        assert!(cache.get("b.example.com", now).is_none());
        assert!(cache.get("c.example.com", now).is_some());
    }

    #[test]
    fn dns_cache_get_purges_expired_entry() {
        let cache = DnsCache::with_capacity(8);
        let now = std::time::Instant::now();
        cache.insert("a.example.com", vec![Ipv4Addr::new(1, 1, 1, 1)], 30, now);
        assert_eq!(cache.len(), 1);

        // Reading past expiry both misses and frees the slot.
        let after = now + std::time::Duration::from_secs(31);
        assert!(cache.get("a.example.com", after).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn is_truncated_detects_tc_flag() {
        let query = build_a_query("example.com");
        // A normal A response is not truncated.
        let resp = build_a_response(&query, Ipv4Addr::new(1, 2, 3, 4)).unwrap();
        assert!(!is_truncated(&resp));

        // Set the TC bit (header byte 2, 0x02).
        let mut truncated = resp.clone();
        truncated[2] |= 0x02;
        assert!(is_truncated(&truncated));

        // Too-short buffers are never "truncated".
        assert!(!is_truncated(&[0x00, 0x01]));
    }

    #[test]
    fn parse_resolv_conf_empty_when_no_nameservers() {
        assert!(parse_resolv_conf("# nothing here\nsearch lan\n").is_empty());
    }

    #[test]
    fn build_servfail_sets_server_failure_rcode() {
        let query = build_a_query("registry.npmjs.org");
        let resp = build_servfail_response(&query).unwrap();

        let packet = Packet::parse(&resp).unwrap();
        assert_eq!(packet.rcode(), simple_dns::RCODE::ServerFailure);
        // The reply must echo the query id so the guest resolver matches it.
        assert_eq!(&resp[0..2], &query[0..2]);
        // QR bit set => it is a response, not a query.
        assert_eq!(resp[2] & 0x80, 0x80);
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
