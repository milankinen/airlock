//! Virtual DNS server.
//!
//! The guest has no real DNS resolver. This server gives each hostname a
//! unique fake IP. The TCP proxy later maps the IP back to the hostname before
//! it sends the connection to the host. Thus the host knows the hostname of
//! each connection.

use std::cell::Cell;
use std::io::Cursor;
use std::net::Ipv4Addr;
use std::rc::Rc;

use scc::HashMap;
use simple_dns::{CLASS, Packet, PacketFlag, QTYPE, Question, ResourceRecord, TYPE, rdata};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// UDP listen address of the server (on loopback).
const LISTEN_ADDR: &str = "10.0.0.1:53";
/// First IP to allocate.
const IP_BASE: u32 = 0x0A020001; // 10.2.0.1

/// Two-way mapping between hostnames and fake IPs. Gives the IPs in
/// sequence from the `10.2.0.0/16` range.
pub struct DnsState {
    host_to_ip: HashMap<String, Ipv4Addr>,
    ip_to_host: HashMap<Ipv4Addr, String>,
    next_ip: Cell<u32>,
}

impl DnsState {
    /// Create an empty DNS state. The first allocated IP is `10.2.0.1`.
    pub fn new() -> Self {
        Self {
            host_to_ip: HashMap::new(),
            ip_to_host: HashMap::new(),
            next_ip: Cell::new(IP_BASE),
        }
    }

    /// Return the IP for `hostname`. Allocates a new IP for a new hostname.
    /// `localhost` and `admin.airlock` always get `127.0.0.1`.
    pub fn allocate(&self, hostname: &str) -> Ipv4Addr {
        if hostname == "localhost" || hostname == "admin.airlock" {
            // `admin.airlock` is reserved for the in-VM admin HTTP service.
            // Loopback traffic does not go through the TCP proxy, so the
            // request goes directly to the server on `127.0.0.1:80`.
            return Ipv4Addr::LOCALHOST;
        }
        if let Some(entry) = self.host_to_ip.get_sync(hostname) {
            return *entry.get();
        }
        let ip = Ipv4Addr::from(self.next_ip.get());
        self.next_ip.set(self.next_ip.get() + 1);
        let _ = self.host_to_ip.insert_sync(hostname.to_string(), ip);
        let _ = self.ip_to_host.insert_sync(ip, hostname.to_string());
        debug!("dns: {hostname} -> {ip}");
        ip
    }

    /// Map a fake IP back to its hostname. Returns `None` for an unknown IP.
    pub fn reverse(&self, ip: Ipv4Addr) -> Option<String> {
        self.ip_to_host.get_sync(&ip).map(|e| e.get().clone())
    }
}

/// Bind the DNS UDP socket and start the server in a local task.
/// Args:
///  - `state`: Shared hostname/IP mapping
///
/// Returns:
///   Error if the bind fails.
pub async fn start(state: Rc<DnsState>) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(LISTEN_ADDR).await?;
    info!("dns listening on {LISTEN_ADDR}");
    tokio::task::spawn_local(async move {
        if let Err(e) = serve(socket, state).await {
            warn!("dns server failed: {e}");
        }
    });
    Ok(())
}

/// Answer DNS queries on `socket` until a socket error occurs.
async fn serve(socket: UdpSocket, state: Rc<DnsState>) -> anyhow::Result<()> {
    let mut buf = [0u8; 512];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        debug!("dns query from {addr}: {len} bytes");
        if let Some(response) = handle_query(&buf[..len], &state) {
            let _ = socket.send_to(&response, addr).await;
        }
    }
}

/// Make the reply to one DNS query. Only the first question gets an answer,
/// and only A queries get an IP. Returns `None` for a query that is not
/// valid.
fn handle_query(data: &[u8], state: &DnsState) -> Option<Vec<u8>> {
    let query = Packet::parse(data).ok()?;
    let question = query.questions.first()?;
    let hostname = question.qname.to_string();
    let hostname = hostname.strip_suffix('.').unwrap_or(&hostname);

    let mut reply = Packet::new_reply(query.id());
    reply.set_flags(PacketFlag::RECURSION_AVAILABLE);
    reply.questions.push(Question::new(
        question.qname.clone(),
        question.qtype,
        question.qclass,
        false,
    ));

    if question.qtype == QTYPE::TYPE(TYPE::A) {
        let ip = state.allocate(hostname);
        reply.answers.push(ResourceRecord::new(
            question.qname.clone(),
            CLASS::IN,
            300,
            rdata::RData::A(rdata::A::from(ip)),
        ));
    }
    // AAAA and others: return a response with no answers

    let mut out = Cursor::new(Vec::with_capacity(512));
    reply.write_compressed_to(&mut out).ok()?;
    Some(out.into_inner())
}
