//! Measures the PC side of the delay: plays short clicks through galdr's playback path (queue,
//! resampler, output device) into a virtual cable and times when they arrive on its recording side.
//!
//! cargo run --release --example latency -- "CABLE Input (VB-Audio Virtual Cable)" "CABLE Output (VB-Audio Virtual Cable)" [buffer_ms]

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use galdr::audio::{self, PcmQueue};

const FRAME: usize = 960; // 20 ms, like the phone's packets
const CLICKS: usize = 10;
const CLICK_EVERY: usize = 25; // frames, 0.5 s
const WARMUP: usize = 50; // frames, 1 s

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (out_name, in_name) = (&args[1], &args[2]);
    let buffer_ms: u32 = args.get(3).map_or(Ok(60), |a| a.parse())?;
    let host = cpal::default_host();
    let input = host
        .input_devices()?
        .find(|d| {
            d.description()
                .map(|x| x.name() == in_name)
                .unwrap_or(false)
        })
        .context("input not found")?;

    // When each click reached the recording side: callback time minus the samples after it.
    let arrivals = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let cfg = input.default_input_config()?;
    let (channels, rate) = (cfg.channels() as usize, cfg.sample_rate() as f64);
    let sink = arrivals.clone();
    let mut armed = true;
    let stream = input.build_input_stream(
        cfg.into(),
        move |d: &[f32], _: &cpal::InputCallbackInfo| {
            let now = Instant::now();
            let frames = d.len() / channels;
            for (i, f) in d.chunks(channels).enumerate() {
                let loud = f[0].abs() > 0.3;
                if loud && armed {
                    let after = Duration::from_secs_f64((frames - i) as f64 / rate);
                    sink.lock().unwrap().push(now - after);
                    armed = false;
                } else if !loud && f[0].abs() < 0.01 {
                    armed = true;
                }
            }
        },
        |e| eprintln!("{e}"),
        None,
    )?;
    stream.play()?;

    let queue = PcmQueue::new(buffer_ms);
    let _play = audio::start_playback(Some(out_name), queue.clone())?.0;
    let mut sent = Vec::new();
    let start = Instant::now();
    for n in 0..WARMUP + CLICKS * CLICK_EVERY {
        let mut frame = vec![0f32; FRAME];
        let click = n >= WARMUP && (n - WARMUP).is_multiple_of(CLICK_EVERY);
        if click {
            frame[..48].fill(0.8); // 1 ms
        }
        // Paced by the clock, like packets from the phone.
        let due = start + Duration::from_millis(20 * n as u64);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        queue.push(&frame);
        if click {
            sent.push(Instant::now());
        }
    }
    thread::sleep(Duration::from_millis(500));
    drop(stream);

    let got = arrivals.lock().unwrap().clone();
    let mut delays: Vec<f64> = sent
        .iter()
        .filter_map(|s| {
            got.iter()
                .find(|a| *a > s)
                .map(|a| (*a - *s).as_secs_f64() * 1000.0)
        })
        .collect();
    println!(
        "buffer {buffer_ms} ms: {} of {} clicks arrived",
        delays.len(),
        sent.len()
    );
    delays.sort_by(f64::total_cmp);
    if let (Some(min), Some(max)) = (delays.first(), delays.last()) {
        println!(
            "  delay min {min:.0} ms, median {:.0} ms, max {max:.0} ms",
            delays[delays.len() / 2]
        );
    }
    Ok(())
}
