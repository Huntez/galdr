//! Playback: a latency-bounded PCM queue drained by a cpal output stream.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::opus::SAMPLE_RATE;

/// 48 kHz mono samples shared between the network thread and the audio callback.
pub struct PcmQueue {
    inner: Mutex<QueueState>,
    target: usize,
}

struct QueueState {
    samples: VecDeque<f32>,
    priming: bool,
}

impl PcmQueue {
    pub fn new(latency_ms: u32) -> Arc<Self> {
        let target = (SAMPLE_RATE * latency_ms / 1000) as usize;
        Arc::new(Self {
            inner: Mutex::new(QueueState {
                samples: VecDeque::with_capacity(target * 4),
                priming: true,
            }),
            target,
        })
    }

    pub fn push(&self, pcm: &[f32]) {
        let mut q = self.inner.lock().unwrap();
        q.samples.extend(pcm);
        // Last resort when the phone runs far ahead (e.g. after a hiccup); normally the rate
        // nudge in `Resampler` keeps the queue near its target.
        let limit = self.target * 2;
        if q.samples.len() > limit {
            let excess = q.samples.len() - self.target;
            q.samples.drain(..excess);
        }
    }

    pub fn buffered_ms(&self) -> u32 {
        (self.inner.lock().unwrap().samples.len() as u64 * 1000 / SAMPLE_RATE as u64) as u32
    }

    /// Buffered audio relative to the target: 1.0 = on target.
    fn level(&self) -> f64 {
        self.inner.lock().unwrap().samples.len() as f64 / self.target.max(1) as f64
    }

    /// Fills `out` with consecutive samples; silence while (re)filling after an underrun.
    fn pop_into(&self, out: &mut [f32]) {
        let mut q = self.inner.lock().unwrap();
        if q.priming && q.samples.len() >= self.target {
            q.priming = false;
        }
        for s in out.iter_mut() {
            *s = if q.priming {
                0.0
            } else {
                q.samples.pop_front().unwrap_or(0.0)
            };
        }
        if q.samples.is_empty() {
            q.priming = true;
        }
    }
}

pub fn list_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    for d in host.output_devices()? {
        names.push(d.description()?.name().to_string());
    }
    Ok(names)
}

pub fn list_input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    for d in host.input_devices()? {
        names.push(d.description()?.name().to_string());
    }
    Ok(names)
}

/// Name fragments of virtual audio cables: what is played into them is heard on their microphone.
/// Steam's "Streaming Microphone" is left out on purpose: its two sides use different formats
/// (stereo 48 kHz in, mono 44.1 kHz out) and it copies samples unconverted, so voice comes out
/// slowed down ~2.2x.
const VIRTUAL_CABLE_HINTS: &[&str] = &["virtual", "cable", "voicemeeter"];

/// An output that other programs see as a microphone.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualMic {
    /// Output device galdr plays into.
    pub output: String,
    /// The matching recording device, as other programs list it.
    pub microphone: String,
}

/// Pairs virtual-cable outputs with their recording side by the shared `(...)` part of the name,
/// e.g. `CABLE Input (VB-Audio Virtual Cable)` -> `CABLE Output (VB-Audio Virtual Cable)`.
/// One entry per microphone: VB-CABLE also has `CABLE In 16ch` feeding the same one, and the
/// plain output (no channel count in its name) is kept.
pub fn find_virtual_mics(outputs: &[String], inputs: &[String]) -> Vec<VirtualMic> {
    fn suffix(name: &str) -> Option<&str> {
        name.rfind('(').map(|i| &name[i..])
    }
    let multichannel = |o: &String| o[..o.rfind('(').unwrap_or(o.len())].contains(char::is_numeric);
    let mut ordered: Vec<&String> = outputs.iter().collect();
    ordered.sort_by_key(|o| multichannel(o)); // stable: plain outputs first, order kept
    let mut mics: Vec<VirtualMic> = ordered
        .into_iter()
        .filter(|o| {
            let l = o.to_lowercase();
            VIRTUAL_CABLE_HINTS.iter().any(|h| l.contains(h))
        })
        .filter_map(|o| {
            let s = suffix(o)?;
            let mic = inputs.iter().find(|i| suffix(i) == Some(s))?;
            Some(VirtualMic {
                output: o.clone(),
                microphone: mic.clone(),
            })
        })
        .collect();
    let mut seen = Vec::new();
    mics.retain(|m| {
        let new = !seen.contains(&m.microphone);
        seen.push(m.microphone.clone());
        new
    });
    // Back to the system's order.
    mics.sort_by_key(|m| outputs.iter().position(|o| *o == m.output));
    mics
}

/// Opens the output named `name` (exact, else case-insensitive substring), or the default one.
pub fn start_playback(name: Option<&str>, queue: Arc<PcmQueue>) -> Result<(Stream, String)> {
    let host = cpal::default_host();
    let device = match name {
        Some(want) => {
            let named = |d: &cpal::Device| {
                d.description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_default()
            };
            let lower = want.to_lowercase();
            let mut devices: Vec<_> = host.output_devices()?.collect();
            let exact = devices.iter().position(|d| named(d) == want);
            let index = exact
                .or_else(|| {
                    devices
                        .iter()
                        .position(|d| named(d).to_lowercase().contains(&lower))
                })
                .with_context(|| {
                    format!("no output device matching {want:?} (see `galdr devices`)")
                })?;
            devices.swap_remove(index)
        }
        None => host
            .default_output_device()
            .context("no default output device")?,
    };
    let device_name = device.description()?.name().to_string();
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, queue)?,
        SampleFormat::I16 => build::<i16>(&device, config, queue)?,
        SampleFormat::I32 => build::<i32>(&device, config, queue)?,
        SampleFormat::U16 => build::<u16>(&device, config, queue)?,
        other => bail!("unsupported output sample format {other}"),
    };
    stream.play()?;
    Ok((stream, device_name))
}

fn build<T>(device: &cpal::Device, config: StreamConfig, queue: Arc<PcmQueue>) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
    let mut scratch = Vec::new();
    let stream = device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = out.len() / channels;
            resampler.fill(frames, &queue, &mut scratch);
            for (frame, &v) in out.chunks_mut(channels).zip(&scratch) {
                frame.fill(T::from_sample(v));
            }
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?;
    Ok(stream)
}

/// The phone's and the output device's clocks never run at exactly the same speed, so the queue
/// would slowly fill up or run dry. Playing up to this much faster or slower keeps it near its
/// target; 0.5% is far below what an ear notices.
const MAX_RATE_NUDGE: f64 = 0.005;

/// Linear-interpolating rate converter from the 48 kHz queue to the device rate, nudged to keep
/// the queue at its target.
struct Resampler {
    step: f64,
    frac: f64,
    prev: f32,
    cur: f32,
    input: Vec<f32>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            frac: 0.0,
            prev: 0.0,
            cur: 0.0,
            input: Vec::new(),
        }
    }

    fn fill(&mut self, frames: usize, queue: &PcmQueue, out: &mut Vec<f32>) {
        out.clear();
        if frames == 0 {
            return;
        }
        // Proportional to how far off target the queue is: 50% over plays 0.25% faster.
        let nudge = ((queue.level() - 1.0) * MAX_RATE_NUDGE).clamp(-MAX_RATE_NUDGE, MAX_RATE_NUDGE);
        let step = self.step * (1.0 + nudge);
        // Pull exactly the input this callback consumes (one per frac wrap), in one lock.
        let needed = (self.frac + step * (frames - 1) as f64).floor() as usize;
        self.input.resize(needed, 0.0);
        queue.pop_into(&mut self.input);
        let mut next = self.input.iter().copied();
        for _ in 0..frames {
            while self.frac >= 1.0 {
                self.prev = self.cur;
                self.cur = next.next().unwrap_or(0.0);
                self.frac -= 1.0;
            }
            out.push(self.prev + (self.cur - self.prev) * self.frac as f32);
            self.frac += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pairs_virtual_cables_with_their_microphones() {
        let outputs = names(&[
            "Speakers (High Definition Audio Device)",
            "Speakers (Steam Streaming Microphone)",
            "Speakers (Steam Streaming Speakers)",
            "CABLE In 16ch (VB-Audio Virtual Cable)",
            "CABLE Input (VB-Audio Virtual Cable)",
        ]);
        let inputs = names(&[
            "Microphone (High Definition Audio Device)",
            "Microphone (Steam Streaming Microphone)",
            "CABLE Output (VB-Audio Virtual Cable)",
        ]);
        assert_eq!(
            find_virtual_mics(&outputs, &inputs),
            vec![VirtualMic {
                output: "CABLE Input (VB-Audio Virtual Cable)".into(),
                microphone: "CABLE Output (VB-Audio Virtual Cable)".into(),
            }]
        );
    }
}
