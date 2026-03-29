use std::net::UdpSocket;

use simple_dns::Packet;
use smoltcp::wire::IpEndpoint;
use tokio::sync::mpsc;
use tracing::debug;

use crate::config::ProxyConfig;
use crate::stack::StackCommand;

/// Handle a DNS query from the guest.
///
/// Resolves the query on the host and sends the response back via the stack.
pub async fn handle_dns_query(
    src: IpEndpoint,
    payload: Vec<u8>,
    cmd_tx: mpsc::UnboundedSender<StackCommand>,
    config: &ProxyConfig,
) {
    let response = match resolve_query(&payload, &config).await {
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

async fn resolve_query(query_bytes: &[u8], config: &ProxyConfig) -> anyhow::Result<Vec<u8>> {
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

    Ok(response_bytes)
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

fn build_refused_response(query_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    build_response_with_rcode(query_bytes, 5)
}
