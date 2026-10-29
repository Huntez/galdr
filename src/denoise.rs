//! Noise suppression on the PC, the same whatever the phone.
//!
//! - RNNoise: built in and tiny, 10 ms. Takes out steady background (fans, hum) evenly.
//! - DeepFilterNet 3, its low-latency model (no lookahead): 10 ms. Separates the voice from the
//!   room much better, but on its own leaves the background cut into pieces.
//! - Both, RNNoise first: 20 ms. RNNoise evens the background out, so little is left for
//!   DeepFilterNet to cut up, and DeepFilterNet still isolates the voice.
//!
//! DeepFilterNet's model is a file next to the program ([`MODEL_FILE`]) and cannot move between
//! threads, so every model lives on a thread of its own: audio goes there and comes back
//! cleaned. Loading takes a moment; until then audio passes through untouched.
//!
//! Noise is turned down by a limited amount rather than cut out: removed entirely it drops away
//! between words and comes back with the voice, which sounds choppy.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use nnnoiseless::DenoiseState;

/// DeepFilterNet 3's low-latency model, from its v0.5.6 release; next to the program.
pub const MODEL_FILE: &str = "DeepFilterNet3_ll_onnx.tar.gz";
/// How much quieter noise gets by default, in dB.
pub const DEFAULT_LIMIT_DB: u32 = 24;
/// From here on noise is removed completely.
pub const MAX_LIMIT_DB: u32 = 100;

/// Both models work in 10 ms hops at 48 kHz.
const HOP: usize = 480;
/// How long a packet waits for its cleaned audio; it usually takes 1-2 ms. A stall of the model
/// must not hold up receiving the next packet: that would look like the network delaying it and
/// grow the playback buffer. The late audio follows with a later packet instead.
const WAIT: Duration = Duration::from_millis(3);
/// Packets still at the model after which it counts as too slow for this PC: 0.5 s.
const MAX_IN_FLIGHT: usize = 50;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Model {
    Rnnoise,
    DeepFilterNet,
    #[default]
    Both,
}

impl Model {
    pub fn delay_ms(self) -> u32 {
        match self {
            Model::Rnnoise | Model::DeepFilterNet => 10,
            Model::Both => 20,
        }
    }

    /// Needs [`MODEL_FILE`].
    pub fn needs_file(self) -> bool {
        self != Model::Rnnoise
    }
}

fn model_path() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    Some(dir.join(MODEL_FILE)).filter(|p| p.is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub model: Model,
    /// How much quieter noise gets, in dB; [`MAX_LIMIT_DB`] removes it completely.
    pub limit_db: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: Model::default(),
            limit_db: DEFAULT_LIMIT_DB,
        }
    }
}

pub struct Denoiser {
    model: Model,
    to_model: Sender<Message>,
    from_model: Receiver<Vec<f32>>,
    state: State,
    /// Packets sent to the model whose audio has not come back yet.
    in_flight: usize,
    _timer: FineTimer,
}

/// A 1 ms timer for this process while noise suppression is on: Windows otherwise rounds the
/// [`WAIT`] for the model up to its 15.6 ms tick, and every such wait grew the playback buffer.
struct FineTimer;

impl FineTimer {
    fn new() -> Self {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        FineTimer
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

enum Message {
    Audio(Vec<f32>),
    LimitDb(u32),
    /// Stands for the model stalling.
    #[cfg(test)]
    Stall(Duration),
}

#[derive(PartialEq)]
enum State {
    Loading,
    Ready,
    /// The model did not load or its thread ended: pass audio through.
    Failed,
}

impl Denoiser {
    /// Starts loading the model on its thread.
    pub fn new(settings: Settings) -> Self {
        let (to_model, input) = mpsc::channel::<Message>();
        let (output, from_model) = mpsc::channel();
        let model = settings.model;
        let spawned = thread::Builder::new()
            .name("galdr-denoise".into())
            .spawn(move || {
                let mut net = match Net::load(model) {
                    Ok(net) => net,
                    Err(e) => return eprintln!("noise suppression: {e:#}"),
                };
                net.set_limit_db(settings.limit_db);
                // Empty: loaded.
                if output.send(Vec::new()).is_err() {
                    return;
                }
                let mut hops = Hops::default();
                while let Ok(message) = input.recv() {
                    match message {
                        Message::Audio(pcm) => {
                            if output.send(hops.process(&mut net, &pcm)).is_err() {
                                return;
                            }
                        }
                        Message::LimitDb(db) => net.set_limit_db(db),
                        #[cfg(test)]
                        Message::Stall(time) => thread::sleep(time),
                    }
                }
            });
        let state = if spawned.is_ok() {
            State::Loading
        } else {
            State::Failed
        };
        Self {
            model,
            to_model,
            from_model,
            state,
            in_flight: 0,
            _timer: FineTimer::new(),
        }
    }

    pub fn model(&self) -> Model {
        self.model
    }

    /// Changes the limit; a different model needs a new `Denoiser`.
    pub fn tune(&mut self, settings: Settings) {
        let _ = self.to_model.send(Message::LimitDb(settings.limit_db));
    }

    /// The model could not be loaded (e.g. its file is missing); audio passes through.
    pub fn failed(&self) -> bool {
        self.state == State::Failed
    }

    /// Appends the cleaned audio to `out`: for `pcm` and any earlier packets that were late,
    /// whole 10 ms hops only. Waits for the model at most [`WAIT`].
    pub fn process(&mut self, pcm: &[f32], out: &mut Vec<f32>) {
        if self.state == State::Loading {
            match self.from_model.try_recv() {
                Ok(_) => self.state = State::Ready,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Failed,
            }
        }
        if self.state == State::Ready && self.in_flight >= MAX_IN_FLIGHT {
            eprintln!("noise suppression: too slow on this PC, turned off");
            self.state = State::Failed;
        }
        if self.state != State::Ready || self.to_model.send(Message::Audio(pcm.to_vec())).is_err() {
            if self.state == State::Ready {
                self.state = State::Failed;
            }
            out.extend_from_slice(pcm);
            return;
        }
        self.in_flight += 1;
        let deadline = Instant::now() + WAIT;
        while self.in_flight > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.from_model.recv_timeout(left) {
                Ok(cleaned) => {
                    out.extend_from_slice(&cleaned);
                    self.in_flight -= 1;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    self.state = State::Failed;
                    break;
                }
            }
        }
    }
}

enum Net {
    Rnn {
        state: Box<DenoiseState<'static>>,
        /// The previous hop as it came in: RNNoise's output is one hop behind.
        previous: Vec<f32>,
        /// How much of the removed noise is put back (from the limit).
        keep: f32,
    },
    Dfn(Box<DfTract>),
    /// The first, then the second.
    Chain(Box<Net>, Box<Net>, Vec<f32>),
}

impl Net {
    fn load(model: Model) -> Result<Self> {
        let rnn = || Net::Rnn {
            state: DenoiseState::new(),
            previous: vec![0.0; HOP],
            keep: 0.0,
        };
        let dfn = || -> Result<Net> {
            let path = model_path().with_context(|| format!("{MODEL_FILE} not found"))?;
            let df = DfTract::new(DfParams::new(path)?, &RuntimeParams::default_with_ch(1))?;
            Ok(Net::Dfn(Box::new(df)))
        };
        Ok(match model {
            Model::Rnnoise => rnn(),
            Model::DeepFilterNet => dfn()?,
            Model::Both => Net::Chain(Box::new(rnn()), Box::new(dfn()?), vec![0.0; HOP]),
        })
    }

    fn set_limit_db(&mut self, db: u32) {
        match self {
            Net::Rnn { keep, .. } => {
                *keep = if db >= MAX_LIMIT_DB {
                    0.0
                } else {
                    10f32.powf(-(db as f32) / 20.0)
                };
            }
            Net::Dfn(df) => df.set_atten_lim(db as f32),
            Net::Chain(first, second, _) => {
                first.set_limit_db(db);
                second.set_limit_db(db);
            }
        }
    }

    /// Cleans one hop into `clean`.
    fn process(&mut self, noisy: &[f32], clean: &mut [f32]) {
        match self {
            Net::Rnn {
                state,
                previous,
                keep,
            } => {
                // RNNoise works on 16-bit sample values.
                let scaled: Vec<f32> = noisy.iter().map(|s| s * 32768.0).collect();
                state.process_frame(clean, &scaled);
                for (c, n) in clean.iter_mut().zip(previous.iter()) {
                    let c0 = *c / 32768.0;
                    *c = c0 + (n - c0) * *keep;
                }
                previous.copy_from_slice(noisy);
            }
            Net::Dfn(df) => {
                let shape = (1, noisy.len()); // one channel
                let done = df.process(
                    ArrayView2::from_shape(shape, noisy).expect("shape"),
                    ArrayViewMut2::from_shape(shape, &mut *clean).expect("shape"),
                );
                if done.is_err() {
                    clean.copy_from_slice(noisy);
                }
            }
            Net::Chain(first, second, between) => {
                first.process(noisy, between);
                second.process(between, clean);
            }
        }
    }
}

/// Regroups audio into hops; the rest of a packet waits for the next one.
#[derive(Default)]
struct Hops {
    pending: Vec<f32>,
    hop: Vec<f32>,
}

impl Hops {
    fn process(&mut self, net: &mut Net, pcm: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(pcm);
        self.hop.resize(HOP, 0.0);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut start = 0;
        while self.pending.len() - start >= HOP {
            net.process(&self.pending[start..start + HOP], &mut self.hop);
            out.extend_from_slice(&self.hop);
            start += HOP;
        }
        self.pending.drain(..start);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads `model`, then feeds 2 s of fan-like noise (low-passed; RNNoise leaves white noise
    /// alone, it resembles hissing consonants) in 5 ms packets; returns how much quieter it came
    /// out (energy ratio). None when the model's file is not next to the test binary.
    fn noise_reduction(model: Model) -> Option<f32> {
        if model.needs_file() && model_path().is_none() {
            eprintln!("skipped: {MODEL_FILE} is not next to the test binary");
            return None;
        }
        let settings = Settings {
            model,
            limit_db: MAX_LIMIT_DB,
        };
        let mut d = Denoiser::new(settings);
        let started = Instant::now();
        // Until loaded, audio passes through as it is.
        while d.state == State::Loading {
            let mut out = Vec::new();
            d.process(&[0.5; 240], &mut out);
            if d.state == State::Loading {
                assert_eq!(out, [0.5; 240]);
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "model never loaded"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(d.state == State::Ready);

        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
        };
        let mut low = 0.0;
        let input: Vec<f32> = (0..96_000)
            .map(|_| {
                low = 0.97 * low + 0.03 * noise();
                low * 0.5
            })
            .collect();
        let mut out = Vec::new();
        for packet in input.chunks(240) {
            d.process(packet, &mut out);
        }
        // Anything still at the model (a slow test machine) comes with the next packets.
        while d.in_flight > 0 {
            d.process(&[], &mut out);
        }
        assert_eq!(out.len(), input.len());
        let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32;
        Some(energy(&input[48_000..]) / energy(&out[48_000..]))
    }

    #[test]
    fn a_stalled_model_does_not_hold_up_packets() {
        let mut d = Denoiser::new(Settings {
            model: Model::Rnnoise,
            limit_db: DEFAULT_LIMIT_DB,
        });
        while d.state == State::Loading {
            d.process(&[0.0; HOP], &mut Vec::new());
        }
        let mut out = Vec::new();
        d.to_model
            .send(Message::Stall(Duration::from_millis(100)))
            .unwrap();
        let mut sent = 0;
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(80) {
            let call = Instant::now();
            d.process(&[0.0; HOP], &mut out);
            sent += HOP;
            assert!(
                call.elapsed() < Duration::from_millis(20),
                "waited {:?}",
                call.elapsed()
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(d.in_flight > 0, "the stall should have left packets behind");
        // Once the model is back, the late audio comes along with the next packets.
        thread::sleep(Duration::from_millis(50));
        d.process(&[0.0; HOP], &mut out);
        sent += HOP;
        assert_eq!(d.in_flight, 0);
        assert_eq!(out.len(), sent);
    }

    #[test]
    fn rnnoise_suppresses_noise() {
        let ratio = noise_reduction(Model::Rnnoise).unwrap();
        assert!(ratio > 10.0, "only {ratio}x quieter");
    }

    #[test]
    fn deepfilternet_and_both_suppress_noise() {
        for model in [Model::DeepFilterNet, Model::Both] {
            if let Some(ratio) = noise_reduction(model) {
                assert!(ratio > 100.0, "{model:?}: only {ratio}x quieter");
            }
        }
    }
}
