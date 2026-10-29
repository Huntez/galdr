//! Diagnoses a virtual cable: plays a 440 Hz tone into an output through galdr's playback
//! path, records the matching input, and reports formats, level, the frequency heard and clicks
//! (jumps a clean sine can't make: dropouts, underruns).
//!
//! cargo run --release --example loopback -- "CABLE Input (VB-Audio Virtual Cable)" "CABLE Output (VB-Audio Virtual Cable)"
//!
//! An optional third argument sets the tone's amplitude (default 0.5). A tone at 0.000003
//! (-110 dBFS) is below 16-bit resolution: it only arrives if the path is deeper than 16 bits.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use galdr::audio::{self, PcmQueue};
use galdr::opus::SAMPLE_RATE;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (out_name, in_name) = (&args[1], &args[2]);
    let amplitude: f32 = args.get(3).map_or(Ok(0.5), |a| a.parse())?;
    let host = cpal::default_host();
    let name = |d: &cpal::Device| {
        d.description()
            .map(|x| x.name().to_string())
            .unwrap_or_default()
    };

    let input = host
        .input_devices()?
        .find(|d| name(d) == *in_name)
        .context("input not found")?;
    let out = host
        .output_devices()?
        .find(|d| name(d) == *out_name)
        .context("output not found")?;
    println!("output default: {:?}", out.default_output_config()?);
    println!("input  default: {:?}", input.default_input_config()?);

    // Tone through the same path galdr uses: queue + resampler.
    let queue = PcmQueue::new(60);
    let _play = audio::start_playback(Some(out_name), queue.clone())?.0;
    let feeder = {
        let queue = queue.clone();
        thread::spawn(move || {
            let mut phase = 0f32;
            let start = Instant::now();
            for n in 0..250 {
                let frame: Vec<f32> = (0..960)
                    .map(|_| {
                        phase += 440.0 / SAMPLE_RATE as f32;
                        (phase * std::f32::consts::TAU).sin() * amplitude
                    })
                    .collect();
                // Paced by the clock, like packets from the phone.
                let due = start + Duration::from_millis(20 * n);
                thread::sleep(due.saturating_duration_since(Instant::now()));
                queue.push(&frame);
            }
        })
    };

    let in_cfg = input.default_input_config()?;
    let channels = in_cfg.channels() as usize;
    let rate = in_cfg.sample_rate();
    let recorded = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink = recorded.clone();
    let stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => input.build_input_stream(
            in_cfg.into(),
            move |d: &[f32], _: &cpal::InputCallbackInfo| {
                sink.lock()
                    .unwrap()
                    .extend(d.chunks(channels).map(|f| f[0]))
            },
            |e| eprintln!("{e}"),
            None,
        )?,
        cpal::SampleFormat::I16 => input.build_input_stream(
            in_cfg.into(),
            move |d: &[i16], _: &cpal::InputCallbackInfo| {
                sink.lock()
                    .unwrap()
                    .extend(d.chunks(channels).map(|f| f[0] as f32 / 32768.0))
            },
            |e| eprintln!("{e}"),
            None,
        )?,
        f => anyhow::bail!("input format {f}"),
    };
    stream.play()?;
    feeder.join().unwrap();
    drop(stream);

    let s = recorded.lock().unwrap();
    let tail = &s[s.len() / 2..];
    let peak = tail.iter().fold(0f32, |m, x| m.max(x.abs()));
    let crossings = tail
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    let secs = tail.len() as f32 / rate as f32;
    // A sine's second difference stays below amplitude * (2 pi f / rate)^2; 10x that is a click.
    let bend = amplitude * (std::f32::consts::TAU * 440.0 / rate as f32).powi(2) * 10.0;
    let clicks = tail
        .windows(3)
        .filter(|w| (w[0] - 2.0 * w[1] + w[2]).abs() > bend)
        .count();
    println!("recorded {} samples @ {rate} Hz", s.len());
    let db = |x: f32| 20.0 * x.max(1e-12).log10();
    println!(
        "peak {peak:.7} = {:.1} dBFS (sent {amplitude} = {:.1} dBFS), frequency ~{:.0} Hz (sent 440), clicks {clicks} in {secs:.1} s",
        db(peak),
        db(amplitude),
        crossings as f32 / secs
    );
    Ok(())
}
