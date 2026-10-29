//! galdr command line.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use galdr::adb;
use galdr::audio;
use galdr::discovery;
use galdr::engine::{Config, Engine, Output, State};
use galdr::protocol::{DEFAULT_CONTROL_PORT, DEFAULT_MEDIA_PORT};

const STATS_PERIOD: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(version, about = "Use your phone as a PC microphone over Wi-Fi or USB")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect to the phone over Wi-Fi or USB and play its microphone
    Connect(ConnectArgs),
    /// List audio output devices
    Devices,
    /// List Galdr phones on the local network and on USB (press Start in the phone app first)
    Find,
    /// Lower VB-CABLE's own buffer (less delay); asks for administrator rights
    #[cfg(windows)]
    TuneCable,
}

#[derive(clap::Args)]
struct ConnectArgs {
    /// Phone IP address shown in the Galdr phone app, or `usb` (the phone on the cable) or
    /// `usb:<serial>` (one of several, see `galdr find`)
    phone: String,
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    control_port: u16,
    /// Local UDP port the phone streams audio to
    #[arg(long, default_value_t = DEFAULT_MEDIA_PORT)]
    media_port: u16,
    /// Output device (substring of its name), e.g. a virtual cable "CABLE Input"
    #[arg(long)]
    device: Option<String>,
    /// Also play to the default speakers (listen to yourself)
    #[arg(long)]
    monitor: bool,
    /// Playback buffer in milliseconds (where the adaptive buffer starts)
    #[arg(long, default_value_t = 60)]
    latency_ms: u32,
    /// Keep the buffer at --latency-ms instead of sizing it to the network
    #[arg(long)]
    fixed_buffer: bool,
    /// Also record what is received to a WAV file
    #[arg(long)]
    wav: Option<PathBuf>,
    /// Do not play audio (useful with --wav)
    #[arg(long)]
    no_play: bool,
    /// Stop after this many seconds
    #[arg(long)]
    seconds: Option<u64>,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Connect(args) => connect(args),
        Command::Find => {
            let phones = discovery::find(Duration::from_secs(2))?;
            let usb = adb::devices();
            if phones.is_empty() && usb.is_empty() {
                println!(
                    "No phone found. Press Start in Galdr on the phone, same Wi-Fi as this PC."
                );
            }
            for p in phones {
                println!("{}  {}  (port {})", p.addr, p.name, p.control_port);
            }
            for d in usb {
                let note = if d.authorized {
                    ""
                } else {
                    "  (allow USB debugging on the phone)"
                };
                println!("usb:{}  {}{note}", d.serial, d.model);
            }
            Ok(())
        }
        Command::Devices => {
            let outputs = audio::list_output_devices()?;
            println!("Outputs:");
            for name in &outputs {
                println!("  {name}");
            }
            let inputs = audio::list_input_devices()?;
            let mics = audio::find_virtual_mics(&outputs, &inputs);
            if !mics.is_empty() {
                println!("Virtual microphones (--device <output>):");
                for m in mics {
                    println!("  {}  ->  {}", m.output, m.microphone);
                }
            }
            Ok(())
        }
        #[cfg(windows)]
        Command::TuneCable => tune_cable(),
    }
}

#[cfg(windows)]
fn tune_cable() -> Result<()> {
    use galdr::cable::{self, NeedsAdmin, Tuned};
    if !cable::installed() {
        anyhow::bail!("VB-CABLE is not installed");
    }
    // Without administrator rights this asks Windows to run the same command elevated.
    let result = match cable::tune() {
        Err(e) if e.is::<NeedsAdmin>() => cable::tune_elevated("tune-cable"),
        result => result,
    };
    match &result {
        Ok(Tuned::Restarted) => println!(
            "VB-CABLE buffer set to {} samples and the cable restarted.",
            cable::LOW_LATENCY
        ),
        Ok(Tuned::RebootNeeded) => println!(
            "VB-CABLE buffer set to {} samples; restart the PC to apply it.",
            cable::LOW_LATENCY
        ),
        Err(e) => eprintln!("error: {e}"),
    }
    std::process::exit(cable::exit_code(&result));
}

fn connect(args: ConnectArgs) -> Result<()> {
    let (phone, usb) = match args.phone.as_str() {
        "usb" => {
            let serial = adb::devices()
                .into_iter()
                .find(|d| d.authorized)
                .context("no phone on USB with USB debugging allowed (see `galdr find`)")?
                .serial;
            (Ipv4Addr::LOCALHOST.into(), Some(serial))
        }
        p => match p.strip_prefix("usb:") {
            Some(serial) => (Ipv4Addr::LOCALHOST.into(), Some(serial.to_string())),
            None => (
                p.parse::<IpAddr>()
                    .with_context(|| format!("{p:?} is neither an IP address nor usb"))?,
                None,
            ),
        },
    };
    let mut outputs = Vec::new();
    let mut monitor = false;
    if !args.no_play {
        let main = match &args.device {
            Some(d) => Output::Device(d.clone()),
            None => Output::Default,
        };
        monitor = args.monitor && main != Output::Default;
        outputs.push(main);
    }
    let engine = Engine::start(Config {
        phone,
        usb,
        control_port: args.control_port,
        media_port: args.media_port,
        outputs,
        monitor,
        latency_ms: args.latency_ms,
        adaptive_buffer: !args.fixed_buffer,
        wav: args.wav,
    });
    let deadline = args
        .seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut last_state = None;
    let mut last_stats = Instant::now();

    loop {
        let stats = engine.stats();
        if last_state.as_ref() != Some(&stats.state) {
            match &stats.state {
                State::Connecting => println!("connecting to {}...", args.phone),
                State::Streaming => println!(
                    "streaming from {} -> {}",
                    stats.phone_name.as_deref().unwrap_or("the phone"),
                    if stats.outputs.is_empty() {
                        "no playback".to_string()
                    } else {
                        stats.outputs.join(", ")
                    }
                ),
                State::Silent => println!("phone sends only silence (volume at 0?)"),
                State::Reconnecting(why) => eprintln!("session ended: {why}; reconnecting..."),
                State::Failed(why) => bail!("{why}"),
                State::Stopped => return Ok(()),
            }
            last_state = Some(stats.state.clone());
        }
        if last_stats.elapsed() >= STATS_PERIOD {
            println!(
                "rx {}  lost {}  bad {}  buffer {}/{}ms  level {:.2}",
                stats.received,
                stats.lost,
                stats.bad,
                stats.buffered_ms,
                stats.target_ms,
                stats.level
            );
            last_stats = Instant::now();
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            engine.stop();
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
