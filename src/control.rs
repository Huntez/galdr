//! Control session: `HELLO`/`WELCOME` with the phone, then pings both ways until one side stops.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::opus::SAMPLE_RATE;
use crate::protocol::*;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PING_PERIOD: Duration = Duration::from_secs(1);
/// Silence on the control connection after which the phone counts as gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ControlSession {
    stream: TcpStream,
    alive: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub welcome: Welcome,
}

impl ControlSession {
    pub fn connect(phone: SocketAddr, media_port: u16) -> Result<Self> {
        let mut stream = TcpStream::connect_timeout(&phone, CONNECT_TIMEOUT)
            .with_context(|| format!("cannot reach phone at {phone}"))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(IDLE_TIMEOUT))?;

        write_message(
            &mut stream,
            MSG_HELLO,
            &hello_payload(media_port, &pc_name()),
        )?;
        let welcome = loop {
            let (kind, payload) = read_message(&mut stream).context("no answer to HELLO")?;
            match kind {
                MSG_WELCOME => break parse_welcome(&payload).context("malformed WELCOME")?,
                MSG_REJECT => bail!("phone refused: {}", String::from_utf8_lossy(&payload)),
                _ => continue, // e.g. an early PING
            }
        };
        if welcome.codec != CODEC_OPUS
            || welcome.sample_rate != SAMPLE_RATE
            || welcome.channels != 1
        {
            bail!(
                "unsupported audio from the phone: codec {}, {} Hz, {} channels",
                welcome.codec,
                welcome.sample_rate,
                welcome.channels
            );
        }

        let alive = Arc::new(AtomicBool::new(true));
        let threads = vec![
            spawn_pinger(stream.try_clone()?, alive.clone()),
            spawn_reader(stream.try_clone()?, alive.clone()),
        ];
        Ok(Self {
            stream,
            alive,
            threads,
            welcome,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for ControlSession {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        // Shutdown ends the reader's blocking read; unpark cuts the pinger's wait short.
        let _ = self.stream.shutdown(Shutdown::Both);
        for t in self.threads.drain(..) {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Shown on the phone as who it streams to.
fn pc_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PC".into())
}

fn spawn_pinger(mut stream: TcpStream, alive: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        while alive.load(Ordering::Relaxed) {
            if write_message(&mut stream, MSG_PING, &[]).is_err() {
                alive.store(false, Ordering::Relaxed);
                return;
            }
            // Woken early by Drop; a spurious wake only sends one extra ping.
            thread::park_timeout(PING_PERIOD);
        }
    })
}

/// The phone pings every second; a read timeout or a closed connection ends the session.
fn spawn_reader(mut stream: TcpStream, alive: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            match read_message(&mut stream) {
                Ok(_) => {} // PING, or a message from a later version
                Err(e) => {
                    if alive.swap(false, Ordering::Relaxed)
                        && e.kind() != io::ErrorKind::UnexpectedEof
                    {
                        eprintln!("control: {e}");
                    }
                    return;
                }
            }
        }
    })
}
