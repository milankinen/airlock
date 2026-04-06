use std::cell::Cell;
use std::io::Cursor;
use std::net::Ipv4Addr;
use std::rc::Rc;

use scc::HashMap;
use simple_dns::{CLASS, Packet, PacketFlag, QTYPE, Question, ResourceRecord, TYPE, rdata};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

const LISTEN_ADDR: &str = "10.0.0.1:53";
const IP_BASE: u32 = 0x0A020001; // 10.2.0.1

pub struct DnsState {
    host_to_ip: HashMap<String, Ipv4Addr>,
    ip_to_host: HashMap<Ipv4Addr, String>,
    next_ip: Cell<u32>,
}

impl DnsState {
    pub fn new() -> Self {
        Self {
            host_to_ip: HashMap::new(),
            ip_to_host: HashMap::new(),
            next_ip: Cell::new(IP_BASE),
        }
    }

    pub fn allocate(&self, hostname: &str) -> Ipv4Addr {
        if hostname == "localhost" {
            return Ipv4Addr::LOCALHOST;
        }
        if let Some(entry) = self.host_to_ip.get(hostname) {
            return *entry.get();
        }
        let ip = Ipv4Addr::from(self.next_ip.get());
        self.next_ip.set(self.next_ip.get() + 1);
        let _ = self.host_to_ip.insert(hostname.to_string(), ip);
        let _ = self.ip_to_host.insert(ip, hostname.to_string());
        debug!("dns: {hostname} -> {ip}");
        ip
    }

    pub fn reverse(&self, ip: Ipv4Addr) -> Option<String> {
        self.ip_to_host.get(&ip).map(|e| e.get().clone())
    }
}

pub fn start(state: Rc<DnsState>) {
    tokio::task::spawn_local(async move {
        if let Err(e) = serve(state).await {
            warn!("dns server failed: {e}");
        }
    });
}

async fn serve(state: Rc<DnsState>) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(LISTEN_ADDR).await?;
    debug!("dns listening on {LISTEN_ADDR}");

    let mut buf = [0u8; 512];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        debug!("dns query from {addr}: {len} bytes");
        if let Some(response) = handle_query(&buf[..len], &state) {
            let _ = socket.send_to(&response, addr).await;
        }
    }
}

fn handle_query(data: &[u8], state: &DnsState) -> Option<Vec<u8>> {
    let query = Packet::parse(data).ok()?;
    let question = query.questions.first()?;
    let hostname = question.qname.to_string();
    // strip trailing dot if present
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
    // AAAA and others: return empty response (no answers)

    let mut out = Cursor::new(Vec::with_capacity(512));
    reply.write_compressed_to(&mut out).ok()?;
    Some(out.into_inner())
}
