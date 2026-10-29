//! Finds Galdr phones on the local network (see docs/PROTOCOL.md, "Discovery") and on USB
//! (through adb, see [`crate::adb`]).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::adb;
use crate::protocol::{
    DEFAULT_CONTROL_PORT, DISCOVERY_PORT, discovery_request, parse_discovery_reply,
};

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
    /// On the network; localhost for a USB phone.
    pub addr: IpAddr,
    pub control_port: u16,
    /// adb serial of a phone on a USB cable.
    pub usb: Option<String>,
}

impl Phone {
    /// Tells phones apart, also across runs: the IP, or `usb:<serial>`.
    pub fn key(&self) -> String {
        match &self.usb {
            Some(serial) => format!("usb:{serial}"),
            None => self.addr.to_string(),
        }
    }
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
        usb: None,
    })
}

/// Phones on USB, described once per serial (each description is two adb calls).
fn usb_phones(names: &mut HashMap<String, (String, String)>) -> Vec<Phone> {
    adb::devices()
        .into_iter()
        .map(|d| {
            let (name, details) = if d.authorized {
                names
                    .entry(d.serial.clone())
                    .or_insert_with(|| {
                        let (name, version) = adb::describe(&d.serial);
                        let details = match version {
                            Some(v) => format!("Android {v}   ·   USB"),
                            None => "USB".into(),
                        };
                        (name.unwrap_or_else(|| d.model.clone()), details)
                    })
                    .clone()
            } else {
                (
                    d.model.clone(),
                    "USB   ·   allow USB debugging on the phone".into(),
                )
            };
            Phone {
                name,
                details,
                addr: Ipv4Addr::LOCALHOST.into(),
                // adb reaches the phone's default port; a changed one is set under Advanced.
                control_port: DEFAULT_CONTROL_PORT,
                usb: Some(d.serial),
            }
        })
        .collect()
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
    usb: Arc<Mutex<Vec<Phone>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Discovery {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(HashMap::new()));
        let usb = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        let network = {
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
        };
        threads.extend(network.ok());
        // Its own thread: the first adb call can take seconds while adb starts its server.
        if adb::available() {
            let (stop, usb) = (stop.clone(), usb.clone());
            let cable = thread::Builder::new()
                .name("galdr-usb".into())
                .spawn(move || {
                    let mut names = HashMap::new();
                    while !stop.load(Ordering::Relaxed) {
                        *usb.lock().unwrap() = usb_phones(&mut names);
                        let next = Instant::now() + ROUND;
                        while Instant::now() < next && !stop.load(Ordering::Relaxed) {
                            thread::sleep(POLL);
                        }
                    }
                });
            threads.extend(cable.ok());
        }
        Self {
            stop,
            seen,
            usb,
            threads,
        }
    }

    /// Phones that answered recently and those on USB, by name.
    pub fn phones(&self) -> Vec<Phone> {
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, (_, at)| at.elapsed() < FORGET_AFTER);
        let mut phones: Vec<Phone> = seen.values().map(|(p, _)| p.clone()).collect();
        phones.extend(self.usb.lock().unwrap().iter().cloned());
        phones.sort_by(|a, b| a.name.cmp(&b.name).then(a.key().cmp(&b.key())));
        phones
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
