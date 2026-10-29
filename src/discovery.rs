//! Finds Galdr phones on the local network (see docs/PROTOCOL.md, "Discovery").

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::protocol::{DISCOVERY_PORT, discovery_request, parse_discovery_reply};

/// How often the background search asks again.
const ROUND: Duration = Duration::from_secs(2);
/// A phone that stops answering for this long drops off the list.
const FORGET_AFTER: Duration = Duration::from_secs(6);
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phone {
    pub name: String,
    /// e.g. "Android 16"; may be empty.
    pub details: String,
    pub addr: IpAddr,
    pub control_port: u16,
}

/// Broadcast addresses of the local IPv4 networks, plus the limited broadcast.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = vec![Ipv4Addr::BROADCAST];
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = &iface.addr
            && !iface.is_loopback()
            && iface.is_oper_up()
            && let Some(b) = v4.broadcast
            && !targets.contains(&b)
        {
            targets.push(b);
        }
    }
    targets
}

fn ask(socket: &UdpSocket) {
    let request = discovery_request();
    for target in broadcast_targets() {
        // A network that refuses broadcasts just finds nothing.
        let _ = socket.send_to(&request, (target, DISCOVERY_PORT));
    }
}

fn open_socket() -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(POLL))?;
    Ok(socket)
}

fn receive(socket: &UdpSocket) -> Option<Phone> {
    let mut buf = [0u8; 512];
    let (n, from): (usize, SocketAddr) = socket.recv_from(&mut buf).ok()?;
    let reply = parse_discovery_reply(&buf[..n])?;
    Some(Phone {
        name: reply.name,
        details: reply.details,
        addr: from.ip(),
        control_port: reply.control_port,
    })
}

/// Asks once and collects the answers that arrive within `wait`.
pub fn find(wait: Duration) -> std::io::Result<Vec<Phone>> {
    let socket = open_socket()?;
    ask(&socket);
    let end = Instant::now() + wait;
    let mut phones: Vec<Phone> = Vec::new();
    while Instant::now() < end {
        if let Some(p) = receive(&socket)
            && !phones.contains(&p)
        {
            phones.push(p);
        }
    }
    Ok(phones)
}

/// Keeps searching in the background until dropped.
pub struct Discovery {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<HashMap<IpAddr, (Phone, Instant)>>>,
    thread: Option<JoinHandle<()>>,
}

impl Discovery {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(HashMap::new()));
        let thread = {
            let (stop, seen) = (stop.clone(), seen.clone());
            thread::Builder::new()
                .name("galdr-discovery".into())
                .spawn(move || {
                    let Ok(socket) = open_socket() else { return };
                    while !stop.load(Ordering::Relaxed) {
                        ask(&socket);
                        let next = Instant::now() + ROUND;
                        while Instant::now() < next && !stop.load(Ordering::Relaxed) {
                            if let Some(p) = receive(&socket) {
                                seen.lock().unwrap().insert(p.addr, (p, Instant::now()));
                            }
                        }
                    }
                })
                .ok()
        };
        Self { stop, seen, thread }
    }

    /// Phones that answered recently, by name.
    pub fn phones(&self) -> Vec<Phone> {
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, (_, at)| at.elapsed() < FORGET_AFTER);
        let mut phones: Vec<Phone> = seen.values().map(|(p, _)| p.clone()).collect();
        phones.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
        phones
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
