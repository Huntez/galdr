//! Background receiver: owns playback and keeps a session with the phone alive.
//!
//! Front ends (CLI, GUI) start an [`Engine`], poll [`Engine::stats`] and stop it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::audio::{self, PcmQueue, Playback};
use crate::control::ControlSession;
use crate::jitter::Jitter;
use crate::opus::{Decoder, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use crate::protocol::{DEFAULT_CONTROL_PORT, DEFAULT_MEDIA_PORT, parse_media};
use crate::wav::WavWriter;

/// Upper bound of the adaptive buffer; the lower one is half a packet + 10 ms.
const MAX_BUFFER_MS: u32 = 300;
/// Audio lost in a row that we still conceal; longer gaps are just skipped.
const MAX_CONCEALED_MS: u32 = 100;
const MEDIA_SILENCE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Digital silence this long is shown as such (e.g. the volume on the phone is at 0).
const SILENT_AFTER: Duration = Duration::from_secs(3);
/// Per-packet decay of the level meter (~0.5 s to fall by 90%).
const LEVEL_DECAY: f32 = 0.9;
/// How often blocking waits check for stop/reconnect; bounds how long `Engine::stop` takes.
const POLL: Duration = Duration::from_millis(30);
/// Peak below this (~-80 dBFS) counts as digital silence; silence frames decode to near-zero noise.
const SILENCE_PEAK: f32 = 1e-4;

#[derive(Clone, Debug)]
pub struct Config {
    pub phone: IpAddr,
    pub control_port: u16,
    pub media_port: u16,
    /// Where to send the audio, all at once (e.g. a microphone). Empty = nowhere.
    pub outputs: Vec<Output>,
    /// Also play to the default speakers; can be switched while running with [`Engine::set_monitor`].
    pub monitor: bool,
    /// Playback buffer; with `adaptive_buffer` only where it starts.
    pub latency_ms: u32,
    /// Size the buffer to how unevenly packets arrive (see [`Jitter`]).
    pub adaptive_buffer: bool,
    pub wav: Option<PathBuf>,
}

impl Config {
    pub fn new(phone: IpAddr) -> Self {
        Self {
            phone,
            control_port: DEFAULT_CONTROL_PORT,
            media_port: DEFAULT_MEDIA_PORT,
            outputs: vec![Output::Default],
            monitor: false,
            latency_ms: 60,
            adaptive_buffer: true,
            wav: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// The system default playback device.
    Default,
    /// A playback device by name (exact, else case-insensitive substring).
    Device(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Connecting,
    /// Connected and audio is coming in.
    Streaming,
    /// Connected, but the phone sends only digital silence.
    Silent,
    /// The last session ended; retrying after a short pause.
    Reconnecting(String),
    /// Cannot continue (e.g. no output device); the engine has stopped.
    Failed(String),
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Stats {
    pub state: State,
    /// Names of the devices being played into.
    pub outputs: Vec<String>,
    /// Name the phone gave in `WELCOME`.
    pub phone_name: Option<String>,
    pub received: u64,
    pub lost: u64,
    pub bad: u64,
    pub buffered_ms: u32,
    /// What the buffer is aiming for.
    pub target_ms: u32,
    /// Recent peak, 0.0..=1.0.
    pub level: f32,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            state: State::Connecting,
            outputs: Vec::new(),
            phone_name: None,
            received: 0,
            lost: 0,
            bad: 0,
            buffered_ms: 0,
            target_ms: 0,
            level: 0.0,
        }
    }
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    monitor: Arc<AtomicBool>,
    stats: Arc<Mutex<Stats>>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(config: Config) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = Arc::new(AtomicBool::new(config.monitor));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let worker = Worker {
            config,
            stop: stop.clone(),
            monitor: monitor.clone(),
            stats: stats.clone(),
        };
        let thread = thread::Builder::new()
            .name("galdr-engine".into())
            .spawn(move || worker.run())
            .expect("spawn engine thread");
        Self {
            stop,
            monitor,
            stats,
            thread: Some(thread),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().unwrap().clone()
    }

    /// Turns playback to the default speakers on or off without reconnecting.
    pub fn set_monitor(&self, on: bool) {
        self.monitor.store(on, Ordering::Relaxed);
    }

    /// Stops and waits for the worker, so a WAV file is complete afterwards.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    config: Config,
    stop: Arc<AtomicBool>,
    monitor: Arc<AtomicBool>,
    stats: Arc<Mutex<Stats>>,
}

impl Worker {
    fn run(self) {
        let final_state = match self.run_inner() {
            Ok(()) => State::Stopped,
            Err(e) => State::Failed(format!("{e:#}")),
        };
        self.update(|s| s.state = final_state);
    }

    fn run_inner(&self) -> Result<()> {
        let mut sinks = Sinks {
            latency_ms: self.config.latency_ms,
            ..Sinks::default()
        };
        for output in &self.config.outputs {
            let device = match output {
                Output::Device(d) => Some(d.as_str()),
                Output::Default => None,
            };
            let queue = PcmQueue::new(sinks.latency_ms);
            let (stream, name) = audio::start_playback(device, queue.clone())?;
            sinks.queues.push(queue);
            sinks.streams.push(stream);
            sinks.names.push(name);
        }
        let names = sinks.output_names();
        self.update(|s| s.outputs = names);
        self.sync_monitor(&mut sinks);
        sinks.wav = self
            .config
            .wav
            .as_deref()
            .map(|p| WavWriter::create(p, SAMPLE_RATE))
            .transpose()?;

        while !self.stopped() {
            self.update(|s| {
                s.state = State::Connecting;
                s.phone_name = None;
            });
            let reason = match self.run_session(&mut sinks) {
                Ok(()) => break,
                Err(e) => format!("{e:#}"),
            };
            self.update(|s| {
                s.state = State::Reconnecting(reason);
                s.level = 0.0;
            });
            let resume = Instant::now() + RECONNECT_DELAY;
            while Instant::now() < resume && !self.stopped() {
                self.sync_monitor(&mut sinks);
                thread::sleep(POLL);
            }
        }
        Ok(())
    }

    /// One connection; returns Ok when asked to stop.
    fn run_session(&self, sinks: &mut Sinks) -> Result<()> {
        let cfg = &self.config;
        // Listen before the handshake: the phone starts streaming right after WELCOME.
        let any: IpAddr = match cfg.phone {
            IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        let udp = UdpSocket::bind((any, cfg.media_port)).with_context(|| {
            format!(
                "UDP port {} is busy (is another copy of Galdr running?)",
                cfg.media_port
            )
        })?;
        udp.set_read_timeout(Some(POLL))?;

        let session =
            ControlSession::connect(SocketAddr::new(cfg.phone, cfg.control_port), cfg.media_port)?;
        self.update(|s| {
            s.phone_name = Some(session.welcome.phone_name.clone());
            s.state = State::Streaming;
        });

        let mut decoder = Decoder::new_mono()?;
        let mut pcm = vec![0f32; MAX_FRAME_SAMPLES];
        let mut buf = [0u8; 2048];
        let mut expected: Option<u32> = None;
        let mut last_packet = Instant::now();
        let mut last_sound = Instant::now();
        let mut last_sync = Instant::now();
        // Phones announce 5, 10 or 20 ms; anything odd is treated as the original 20.
        let frame_ms = match session.welcome.frame_ms {
            ms @ 1..=60 => ms as u32,
            _ => 20,
        };
        let frame_samples = (SAMPLE_RATE * frame_ms / 1000) as usize;
        let mut jitter = cfg
            .adaptive_buffer
            .then(|| Jitter::new(frame_ms, cfg.latency_ms, frame_ms / 2 + 10, MAX_BUFFER_MS));
        sinks.set_latency(cfg.latency_ms);

        loop {
            if self.stopped() {
                return Ok(());
            }
            self.sync_monitor(sinks);
            if !session.is_alive() {
                bail!("phone closed the control connection");
            }
            if last_packet.elapsed() > MEDIA_SILENCE_TIMEOUT {
                bail!("no audio for {}s", MEDIA_SILENCE_TIMEOUT.as_secs());
            }
            if let Some(w) = sinks
                .wav
                .as_mut()
                .filter(|_| last_sync.elapsed() >= Duration::from_secs(1))
            {
                w.sync()?;
                last_sync = Instant::now();
            }

            let (n, from) = match udp.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if from.ip().to_canonical() != cfg.phone.to_canonical() {
                continue;
            }
            let Some(packet) = parse_media(&buf[..n]) else {
                self.update(|s| s.bad += 1);
                continue;
            };
            last_packet = Instant::now();

            let mut lost = 0;
            if let Some(want) = expected {
                let ahead = packet.seq.wrapping_sub(want);
                if ahead >= 0x8000_0000 {
                    continue; // late or duplicate
                }
                lost = ahead as u64;
                for _ in 0..ahead.min(MAX_CONCEALED_MS / frame_ms) {
                    let got = decoder.decode(None, &mut pcm[..frame_samples])?;
                    sinks.deliver(&pcm[..got])?;
                }
            }
            expected = Some(packet.seq.wrapping_add(1));
            if let Some(target) = jitter.as_mut().and_then(|j| j.packet(packet.timestamp)) {
                sinks.set_latency(target);
            }

            let (peak, bad) = match decoder.decode(Some(packet.opus), &mut pcm) {
                Ok(got) => {
                    sinks.deliver(&pcm[..got])?;
                    (pcm[..got].iter().fold(0f32, |m, s| m.max(s.abs())), 0)
                }
                Err(_) => (0.0, 1),
            };
            if peak > SILENCE_PEAK {
                last_sound = Instant::now();
            }
            let silent = last_sound.elapsed() >= SILENT_AFTER;
            let buffered = sinks.queues.first().map_or(0, |q| q.buffered_ms());
            let target = sinks.latency_ms;
            self.update(|s| {
                s.received += 1;
                s.lost += lost;
                s.bad += bad;
                s.buffered_ms = buffered;
                s.target_ms = target;
                s.level = (s.level * LEVEL_DECAY).max(peak.min(1.0));
                s.state = if silent {
                    State::Silent
                } else {
                    State::Streaming
                };
            });
        }
    }

    /// Opens or closes the speakers output to match [`Engine::set_monitor`].
    fn sync_monitor(&self, sinks: &mut Sinks) {
        let want = self.monitor.load(Ordering::Relaxed);
        if want == sinks.monitor.is_some() {
            return;
        }
        sinks.monitor = None;
        if want {
            let queue = PcmQueue::new(sinks.latency_ms);
            match audio::start_playback(None, queue.clone()) {
                Ok((stream, name)) => sinks.monitor = Some((stream, queue, name)),
                Err(e) => {
                    eprintln!("speakers: {e:#}");
                    self.monitor.store(false, Ordering::Relaxed);
                }
            }
        }
        let names = sinks.output_names();
        self.update(|s| s.outputs = names);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn update(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.stats.lock().unwrap());
    }
}

/// Everything decoded audio goes to. Lives on the worker thread: cpal streams (the fallback
/// output) must stay on the thread that created them.
#[derive(Default)]
struct Sinks {
    /// Display names of the fixed outputs.
    names: Vec<String>,
    // One queue per device: their clocks drift independently.
    queues: Vec<Arc<PcmQueue>>,
    streams: Vec<Playback>,
    /// Default speakers, switchable at runtime.
    monitor: Option<(Playback, Arc<PcmQueue>, String)>,
    wav: Option<WavWriter>,
    /// Current buffer target of every queue.
    latency_ms: u32,
}

impl Sinks {
    fn output_names(&self) -> Vec<String> {
        let mut names = self.names.clone();
        names.extend(self.monitor.as_ref().map(|m| m.2.clone()));
        names
    }

    fn set_latency(&mut self, ms: u32) {
        self.latency_ms = ms;
        for q in self
            .queues
            .iter()
            .chain(self.monitor.as_ref().map(|m| &m.1))
        {
            q.set_latency_ms(ms);
        }
    }

    fn deliver(&mut self, pcm: &[f32]) -> Result<()> {
        for q in self
            .queues
            .iter()
            .chain(self.monitor.as_ref().map(|m| &m.1))
        {
            q.push(pcm);
        }
        if let Some(w) = &mut self.wav {
            w.write(pcm)?;
        }
        Ok(())
    }
}
