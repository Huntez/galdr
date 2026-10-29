//! galdr window: connect to the phone, pick an output, watch the level. Closes to the tray.

#![windows_subsystem = "windows"]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use galdr::audio;
use galdr::discovery::{Discovery, Phone};
use galdr::engine::{Config, Engine, Output, State, Stats};
use galdr::protocol::{DEFAULT_CONTROL_PORT, DEFAULT_MEDIA_PORT};

const DEFAULT_LATENCY_MS: u32 = 60;

/// Key of settings saved by older builds in eframe's storage (read once to migrate).
const SETTINGS_KEY: &str = "galdr-settings";
const REFRESH: Duration = Duration::from_millis(100);
/// Level (recent peak) above which the tray dot turns green; below `SPEAKING_OFF` it turns
/// grey again. Two thresholds so the dot does not flicker at the edge.
const SPEAKING_ON: f32 = 0.04;
const SPEAKING_OFF: f32 = 0.015;
/// Command-line flag used by the autostart entry: start hidden in the tray.
const TRAY_ARG: &str = "--tray";
/// Run as administrator by "Lower it": set VB-CABLE's buffer (galdr::cable::tune) and exit.
const TUNE_CABLE_ARG: &str = "--tune-cable";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// The phone to connect to: its IP, or `usb:<serial>` (see `Phone::key`).
    phone: String,
    /// What is typed under Enter IP; becomes `phone` on Connect.
    typed_ip: String,
    /// Output device galdr sends audio to so other programs hear it as a microphone
    /// (usually a virtual cable).
    mic: Option<String>,
    /// Also play to the default speakers.
    listen: bool,
    /// Suppress background noise on the PC (galdr::denoise).
    denoise: bool,
    /// How much quieter noise gets, in dB.
    denoise_db: u32,
    denoise_model: DenoiseModel,
    /// Size the buffer to the network; `latency_ms` is then only where it starts.
    auto_buffer: bool,
    latency_ms: u32,
    /// TCP port of the phone app.
    control_port: u16,
    /// Local UDP port the phone streams audio to.
    media_port: u16,
    connect_on_start: bool,
    /// Phones tab: false = search the network, true = type the IP by hand.
    manual_ip: bool,
    /// A phone reachable both ways connects over USB (steadier, less delay) unless Wi-Fi is picked.
    prefer_usb: bool,
    close_to_tray: bool,
    /// A virtual cable was there on some run; until then one that appears is picked by itself.
    cable_seen: bool,
    /// Packets, losses and the buffer under the level meter.
    show_stats: bool,
}

/// Which model suppresses noise (galdr::denoise::Model). The aliases are names from test builds.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum DenoiseModel {
    #[serde(alias = "Light")]
    Rnnoise,
    #[serde(alias = "Strong", alias = "Clicks")]
    DeepFilterNet,
    Both,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            phone: String::new(),
            typed_ip: String::new(),
            mic: None,
            listen: false,
            denoise: false,
            denoise_db: galdr::denoise::DEFAULT_LIMIT_DB,
            denoise_model: DenoiseModel::Both,
            auto_buffer: true,
            latency_ms: DEFAULT_LATENCY_MS,
            control_port: DEFAULT_CONTROL_PORT,
            media_port: DEFAULT_MEDIA_PORT,
            connect_on_start: false,
            close_to_tray: true,
            manual_ip: false,
            prefer_usb: true,
            cable_seen: false,
            show_stats: true,
        }
    }
}

impl Settings {
    fn denoise_settings(&self) -> Option<galdr::denoise::Settings> {
        self.denoise.then_some(galdr::denoise::Settings {
            model: match self.denoise_model {
                DenoiseModel::Rnnoise => galdr::denoise::Model::Rnnoise,
                DenoiseModel::DeepFilterNet => galdr::denoise::Model::DeepFilterNet,
                DenoiseModel::Both => galdr::denoise::Model::Both,
            },
            limit_db: self.denoise_db,
        })
    }

    /// `%APPDATA%\galdr\settings.json`.
    fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("galdr").join("settings.json"))
    }

    fn load() -> Option<Self> {
        let text = std::fs::read_to_string(Self::path()?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Saved on every change, so nothing is lost however the app is closed.
    fn save(&self) {
        let Some(path) = Self::path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(self).unwrap_or_default(),
                )
            });
        if let Err(e) = result {
            eprintln!("saving settings: {e}");
        }
    }
}

/// A destination that other programs see as a microphone.
struct MicOption {
    /// Value stored in [`Settings::mic`].
    key: String,
    /// Short name for the list.
    label: String,
    /// Recording device name, as other programs list it.
    microphone: String,
}

struct Devices {
    outputs: Vec<String>,
    mics: Vec<MicOption>,
}

impl Devices {
    fn scan() -> Self {
        let outputs = audio::list_output_devices().unwrap_or_default();
        let inputs = audio::list_input_devices().unwrap_or_default();
        let mics = audio::find_virtual_mics(&outputs, &inputs)
            .into_iter()
            .map(|m| MicOption {
                key: m.output,
                label: cable_name(&m.microphone),
                microphone: m.microphone,
            })
            .collect();
        Self { outputs, mics }
    }

    fn mic(&self, key: &str) -> Option<&MicOption> {
        self.mics.iter().find(|m| m.key == key)
    }
}

fn output_for(key: &str) -> Output {
    Output::Device(key.to_string())
}

struct GaldrApp {
    settings: Settings,
    /// What is on disk (`None` until first saved); compared each frame to save changes right away.
    saved: Option<Settings>,
    devices: Devices,
    engine: Option<Engine>,
    error: Option<String>,
    quitting: Arc<AtomicBool>,
    tray: Option<TrayIcon>,
    /// What the tray icon currently shows; changed only when it differs.
    tray_look: TrayLook,
    /// An engine being shut down in the background, so Disconnect never blocks the window.
    stopping: Option<JoinHandle<()>>,
    /// Mirrors the Windows autostart entry (the registry is the source of truth).
    autostart: bool,
    /// Started with `--tray`: hide the window on the first frame.
    start_hidden: bool,
    /// Searches for phones on the network and USB for as long as Galdr runs.
    discovery: Option<Discovery>,
    /// What the search found, refreshed once per frame.
    phones: Vec<Phone>,
    cable: CableTune,
}

impl GaldrApp {
    fn new(cc: &eframe::CreationContext<'_>, in_tray: bool) -> Self {
        let settings = Settings::load()
            .or_else(|| cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)))
            .unwrap_or_default();
        apply_style(&cc.egui_ctx);
        let quitting = Arc::new(AtomicBool::new(false));
        let tray = build_tray(&cc.egui_ctx, quitting.clone());
        // Nowhere to hide without a tray icon: then the window stays visible.
        let start_hidden = in_tray && tray.is_some();
        system::on_show_request(cc.egui_ctx.clone());
        let autostart = system::autostart_enabled();
        if autostart {
            // Keeps the entry pointing at this exe if it was moved.
            let _ = system::set_autostart(true);
        }
        let devices = Devices::scan();
        let mut settings = settings;
        // The typed IP used to share `phone` with the phone picked under Search.
        if settings.typed_ip.is_empty() && settings.phone.parse::<IpAddr>().is_ok() {
            settings.typed_ip = settings.phone.clone();
        }
        // Earlier default ports.
        if (settings.control_port, settings.media_port) == (8125, 30000) {
            settings.control_port = DEFAULT_CONTROL_PORT;
            settings.media_port = DEFAULT_MEDIA_PORT;
        }
        // Steam's cable distorts voice (see audio::VIRTUAL_CABLE_HINTS); forget it if picked earlier.
        let forgotten = settings
            .mic
            .as_deref()
            .is_some_and(|m| m.contains("Steam Streaming Microphone"));
        if forgotten || (settings.mic.is_none() && !settings.listen) {
            // First run, or the old choice is gone: a virtual cable if there is one, else the
            // speakers.
            settings.mic = devices.mics.first().map(|m| m.key.clone());
            // Keep "Listen to myself" if it was on; without a cable it is the only output.
            settings.listen = (forgotten && settings.listen) || settings.mic.is_none();
        }
        adopt_new_cable(&mut settings, &devices);
        let mut app = Self {
            saved: None,
            settings,
            devices,
            engine: None,
            error: None,
            quitting,
            tray,
            tray_look: TrayLook::Off,
            stopping: None,
            autostart,
            start_hidden,
            discovery: None,
            phones: Vec::new(),
            cable: CableTune::check(),
        };
        if app.settings.connect_on_start {
            app.connect();
        }
        app
    }

    fn connect(&mut self) {
        let chosen = self.settings.phone.trim();
        let usb = chosen.strip_prefix("usb:").map(str::to_string);
        let phone: IpAddr = match (&usb, chosen.parse()) {
            (Some(_), _) => Ipv4Addr::LOCALHOST.into(),
            (None, Ok(ip)) => ip,
            (None, Err(_)) => {
                self.error = Some("Pick your phone or enter the IP shown in Galdr on it".into());
                return;
            }
        };
        let outputs: Vec<Output> = self.settings.mic.iter().map(|k| output_for(k)).collect();
        if outputs.is_empty() && !self.settings.listen {
            self.error = Some("Choose a microphone or turn on Listen to myself".into());
            return;
        }
        self.error = None;
        let mut config = Config::new(phone);
        config.usb = usb;
        config.outputs = outputs;
        config.monitor = self.settings.listen;
        config.denoise = self.settings.denoise_settings();
        config.latency_ms = self.settings.latency_ms;
        config.adaptive_buffer = self.settings.auto_buffer;
        config.control_port = self.settings.control_port;
        config.media_port = self.settings.media_port;
        // The previous engine must release the UDP port and the driver first.
        self.finish_stopping();
        self.engine = Some(Engine::start(config));
    }

    fn disconnect(&mut self) {
        if let Some(engine) = self.engine.take() {
            self.finish_stopping();
            self.stopping = Some(thread::spawn(move || engine.stop()));
        }
    }

    fn finish_stopping(&mut self) {
        if let Some(t) = self.stopping.take() {
            let _ = t.join();
        }
    }

    /// An engine that failed or stopped no longer counts as running.
    fn running(&self) -> Option<Stats> {
        let stats = self.engine.as_ref()?.stats();
        (!matches!(stats.state, State::Failed(_) | State::Stopped)).then_some(stats)
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, locked: bool) {
        section(ui, "Sound", |ui| {
            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Microphone");
                    ui.add_enabled_ui(!locked, |ui| {
                        ui.horizontal(|ui| {
                            let current = match &self.settings.mic {
                                None => "Off".to_string(),
                                Some(key) => self
                                    .devices
                                    .mic(key)
                                    .map_or_else(|| key.clone(), |m| m.label.clone()),
                            };
                            let refresh_w = 32.0;
                            egui::ComboBox::from_id_salt("mic")
                                .selected_text(current)
                                .width(
                                    (ui.available_width()
                                        - refresh_w
                                        - ui.spacing().item_spacing.x)
                                        .max(80.0),
                                )
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.settings.mic, None, "Off");
                                    for m in &self.devices.mics {
                                        ui.selectable_value(
                                            &mut self.settings.mic,
                                            Some(m.key.clone()),
                                            &m.label,
                                        );
                                    }
                                    ui.separator();
                                    ui.label(RichText::new("Other outputs").small().weak());
                                    for name in &self.devices.outputs {
                                        if self.devices.mic(name).is_none() {
                                            ui.selectable_value(
                                                &mut self.settings.mic,
                                                Some(name.clone()),
                                                name,
                                            );
                                        }
                                    }
                                });
                            if refresh_button(ui, refresh_w)
                                .on_hover_text("Refresh devices")
                                .clicked()
                            {
                                self.devices = Devices::scan();
                                adopt_new_cable(&mut self.settings, &self.devices);
                            }
                        })
                    });
                    ui.end_row();

                    ui.label("");
                    self.mic_hint(ui);
                    ui.end_row();

                    if self.cable_ui_shown() {
                        ui.label("");
                        self.cable_ui(ui, locked);
                        ui.end_row();
                    }

                    ui.label("");
                    if ui
                        .checkbox(&mut self.settings.listen, "Listen to myself")
                        .changed()
                        && let Some(engine) = &self.engine
                    {
                        engine.set_monitor(self.settings.listen);
                    }
                    ui.end_row();

                    ui.label("");
                    let before = self.settings.denoise_settings();
                    ui.checkbox(&mut self.settings.denoise, "Noise suppression")
                        .on_hover_text(
                            "Removes background noise on the PC, the same with any phone. Best \
                             with Noise suppression off on the phone: with both, the voice can \
                             come out dull.",
                        );
                    ui.end_row();

                    if self.settings.denoise {
                        ui.label("");
                        ui.vertical(|ui| {
                            segmented(
                                ui,
                                ui.spacing().interact_size.y,
                                &mut self.settings.denoise_model,
                                [
                                    (
                                        "RNNoise",
                                        DenoiseModel::Rnnoise,
                                        "+10 ms. Takes out steady background such as fans \
                                         evenly; the voice stays as it is.",
                                    ),
                                    (
                                        "DeepFilterNet",
                                        DenoiseModel::DeepFilterNet,
                                        "+10 ms. Separates the voice from the room much \
                                         better, but the background can cut in and out.",
                                    ),
                                    (
                                        "Both",
                                        DenoiseModel::Both,
                                        "+20 ms. RNNoise, then DeepFilterNet: an even \
                                         background and an isolated voice.",
                                    ),
                                ],
                            );
                            let slider = egui::Slider::new(&mut self.settings.denoise_db, 6..=48)
                                .text("quieter")
                                .custom_formatter(|db, _| format!("{db} dB"));
                            ui.add(slider).on_hover_text(
                                "How much quieter the noise gets while you speak. More is \
                                 cleaner, but the background can start to cut in and out.",
                            );
                            if self
                                .engine
                                .as_ref()
                                .is_some_and(|e| e.stats().denoise_failed)
                            {
                                ui.label(
                                    RichText::new(
                                        "The DeepFilterNet model file is missing next to \
                                         Galdr; RNNoise works without it",
                                    )
                                    .small()
                                    .color(RED),
                                );
                            }
                        });
                        ui.end_row();
                    }
                    let after = self.settings.denoise_settings();
                    if after != before
                        && let Some(engine) = &self.engine
                    {
                        engine.set_denoise(after);
                    }
                });
        });
    }
}

impl GaldrApp {
    /// Ports and buffer, folded away by default; locked while connected.
    fn advanced_ui(&mut self, ui: &mut egui::Ui, locked: bool) {
        egui::CollapsingHeader::new(RichText::new("Advanced").size(15.0))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!locked, |ui| {
                    egui::Grid::new("advanced")
                        .num_columns(2)
                        .spacing([14.0, 10.0])
                        .show(ui, |ui| {
                            ui.label("Control port");
                            ui.add(
                                egui::DragValue::new(&mut self.settings.control_port)
                                    .range(1..=65535),
                            )
                            .on_hover_text(format!(
                                "TCP port of the phone app (default {DEFAULT_CONTROL_PORT}); \
                                 filled in when you pick a found phone"
                            ));
                            ui.end_row();

                            ui.label("Media port");
                            ui.add(
                                egui::DragValue::new(&mut self.settings.media_port)
                                    .range(1..=65535),
                            )
                            .on_hover_text(format!(
                                "Local UDP port the phone sends audio to (default {DEFAULT_MEDIA_PORT})"
                            ));
                            ui.end_row();

                            ui.label("Buffer");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.settings.auto_buffer, "Auto")
                                    .on_hover_text(
                                        "As small as your Wi-Fi allows: grows when packets \
                                         arrive late, shrinks slowly when they don't",
                                    );
                                if !self.settings.auto_buffer {
                                    // Leave room for the value box so a narrow window does not
                                    // overflow.
                                    ui.spacing_mut().slider_width =
                                        (ui.available_width() - 70.0).clamp(60.0, 220.0);
                                    ui.add(
                                        egui::Slider::new(&mut self.settings.latency_ms, 20..=300)
                                            .suffix(" ms"),
                                    )
                                    .on_hover_text(
                                        "Lower = less delay, higher = fewer dropouts on weak Wi-Fi",
                                    );
                                }
                            });
                            ui.end_row();

                            ui.label("Statistics");
                            ui.checkbox(&mut self.settings.show_stats, "Show")
                                .on_hover_text(
                                    "While connected: packets, losses and the buffer under the \
                                     level meter",
                                );
                            ui.end_row();
                        });
                    if ui.button("Reset to defaults").clicked() {
                        self.settings.control_port = DEFAULT_CONTROL_PORT;
                        self.settings.media_port = DEFAULT_MEDIA_PORT;
                        self.settings.latency_ms = DEFAULT_LATENCY_MS;
                        self.settings.auto_buffer = true;
                    }
                });
            });
    }

    /// Two tabs: Search lists the phones found on the network and on USB, Enter IP takes an
    /// address by hand. One card per phone, with Wi-Fi or USB to choose when both reach it; while
    /// connected only the live card is shown, with the status.
    fn phones_ui(&mut self, ui: &mut egui::Ui, stats: Option<&Stats>) {
        let connected = stats.is_some();
        section(ui, "Phones", |ui| {
            ui.add_enabled_ui(!connected, |ui| {
                let height = ui.spacing().interact_size.y;
                segmented(
                    ui,
                    height,
                    &mut self.settings.manual_ip,
                    [("Search", false, ""), ("Enter IP", true, "")],
                );
            });
            ui.add_space(4.0);

            let chosen = self.settings.phone.trim().to_string();
            // The phone being streamed from, for when it is not among the cards below
            // (e.g. typed by hand, or its cable was pulled), so the status stays in view.
            let live_card = |key: &str| {
                let usb = key.strip_prefix("usb:").map(str::to_string);
                let phone = Phone {
                    name: String::new(),
                    details: String::new(),
                    addr: key.parse().unwrap_or(Ipv4Addr::LOCALHOST.into()),
                    control_port: self.settings.control_port,
                    usb: usb.clone(),
                };
                PhoneCard {
                    name: stats
                        .and_then(|s| s.phone_name.clone())
                        .unwrap_or_else(|| key.to_string()),
                    details: String::new(),
                    wifi: usb.is_none().then(|| phone.clone()),
                    usb: usb.map(|_| phone),
                }
            };
            let mut cards = PhoneCard::group(&self.phones);
            if connected {
                // Only the phone being streamed from, whichever tab it was picked on.
                cards.retain(|c| c.has(&chosen));
                if cards.is_empty() {
                    cards.push(live_card(&chosen));
                }
            } else if self.settings.manual_ip {
                // Field and button in one row; the card appears once connected.
                cards.clear();
                ui.horizontal(|ui| {
                    let button_w = 90.0;
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.typed_ip)
                            .hint_text("IP shown in Galdr on the phone, e.g. 192.168.1.20")
                            .desired_width(
                                ui.available_width() - button_w - ui.spacing().item_spacing.x,
                            ),
                    );
                    let connect = accent_button("Connect").min_size(egui::vec2(button_w, 0.0));
                    if ui.add(connect).clicked() {
                        self.settings.phone = self.settings.typed_ip.trim().to_string();
                        self.connect();
                    }
                });
            } else if cards.is_empty() {
                ui.label(RichText::new("Searching… press Start in Galdr on the phone").weak());
            }

            for card in &cards {
                let live = stats.filter(|_| card.has(&chosen));
                // The connected way, else the preferred one when both reach the phone.
                let phone = match (&card.wifi, &card.usb) {
                    (Some(w), Some(u)) if live.is_some() => {
                        if u.key() == chosen {
                            u
                        } else {
                            w
                        }
                    }
                    (Some(w), Some(u)) => {
                        if self.settings.prefer_usb {
                            u
                        } else {
                            w
                        }
                    }
                    (Some(p), None) | (None, Some(p)) => p,
                    (None, None) => continue,
                };
                let stroke = if live.is_some() {
                    egui::Stroke::new(1.5, ACCENT)
                } else {
                    ui.visuals().widgets.noninteractive.bg_stroke
                };
                // A shade off the panel so the card stands out, a little more under the pointer.
                let mut frame = egui::Frame::new()
                    .stroke(stroke)
                    .corner_radius(6)
                    .inner_margin(12)
                    .begin(ui);
                {
                    let ui = &mut frame.content_ui;
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&card.name).size(16.0).strong());
                            // Over USB the address is just localhost: not worth showing.
                            let line = [
                                card.details.clone(),
                                match phone.usb {
                                    Some(_) => String::new(),
                                    None => phone.addr.to_string(),
                                },
                            ]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("   ·   ");
                            ui.label(RichText::new(line).small().weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // One width for both, so the button stays put on connect.
                            let wide = egui::vec2(100.0, 0.0);
                            let action = if live.is_some() {
                                let disconnect =
                                    egui::Button::new(RichText::new("Disconnect").size(15.0))
                                        .min_size(wide);
                                let response = ui.add(disconnect);
                                if response.clicked() {
                                    self.disconnect();
                                }
                                response
                            } else {
                                let response = ui.add(accent_button("Connect").min_size(wide));
                                if response.clicked() {
                                    self.settings.phone = phone.key();
                                    self.settings.control_port = phone.control_port;
                                    self.connect();
                                }
                                response
                            };
                            ui.add_space(4.0);
                            if card.wifi.is_some() && card.usb.is_some() && live.is_none() {
                                segmented(
                                    ui,
                                    action.rect.height(),
                                    &mut self.settings.prefer_usb,
                                    [("Wi-Fi", false, ""), ("USB", true, "")],
                                );
                            } else {
                                let way = if phone.usb.is_some() { "USB" } else { "Wi-Fi" };
                                ui.label(RichText::new(way).weak());
                            }
                        });
                    });
                    if let Some(s) = live {
                        ui.add_space(6.0);
                        status_ui(ui, s, self.settings.show_stats);
                    }
                }
                let outer = frame.content_ui.min_rect() + frame.frame.total_margin();
                let shade = if ui.rect_contains_pointer(outer) {
                    0.09
                } else {
                    0.05
                };
                let visuals = ui.visuals();
                frame.frame.fill = visuals
                    .panel_fill
                    .lerp_to_gamma(visuals.text_color(), shade);
                frame.end(ui);
            }
            if stats.is_none()
                && let Some(err) = &self.error
            {
                status_line(ui, RED, err);
            }
        });
    }

    /// Refreshes the phones found; picks the phone when exactly one answers and none was chosen
    /// yet.
    fn update_discovery(&mut self, ctx: &egui::Context) {
        // Kept running throughout (one broadcast and one adb call every 2 s): restarting it on
        // every tab switch or disconnect emptied the list and rebuilt it card by card.
        let discovery = self.discovery.get_or_insert_with(Discovery::start);
        self.phones = discovery.phones();
        if self.engine.is_none()
            && !self.settings.manual_ip
            && self.settings.phone.trim().is_empty()
            && let [only] = self.phones.as_slice()
        {
            self.settings.phone = only.key();
            self.settings.control_port = only.control_port;
        }
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn mic_hint(&self, ui: &mut egui::Ui) {
        let hint = |ui: &mut egui::Ui, text: String| {
            ui.add(egui::Label::new(RichText::new(text).small().weak()).wrap())
        };
        match &self.settings.mic {
            Some(key) => {
                let text = match self.devices.mic(key) {
                    Some(m) => format!("Appears in apps as: {}", m.microphone),
                    None => "Plays into this output; apps will not see it as a microphone".into(),
                };
                hint(ui, text);
            }
            None if self.devices.mics.is_empty() => {
                ui.vertical(|ui| {
                    hint(
                        ui,
                        "No virtual microphone found. Install VB-CABLE (its setup as \
                         administrator, then restart the PC):"
                            .into(),
                    );
                    ui.hyperlink_to(
                        RichText::new("VB-CABLE").small(),
                        "https://vb-audio.com/Cable/",
                    );
                });
            }
            None => {
                hint(ui, "Not used as a microphone".into());
            }
        }
    }
}

/// A virtual cable installed since the last run is picked, as on the first run, and replaces
/// Listen to myself, which was only on for want of one.
fn adopt_new_cable(settings: &mut Settings, devices: &Devices) {
    let Some(cable) = devices.mics.first() else {
        return;
    };
    if !settings.cable_seen && settings.mic.is_none() {
        settings.mic = Some(cable.key.clone());
        settings.listen = false;
    }
    settings.cable_seen = true;
}

/// VB-CABLE's own buffer (see galdr::cable): offered to lower when it is at the large default.
enum CableTune {
    Fine,
    Large,
    /// Waiting for the elevated copy: a message for the user, or an error.
    Waiting(mpsc::Receiver<Result<String, String>>),
    Done(String),
    Failed(String),
}

impl CableTune {
    fn check() -> Self {
        #[cfg(windows)]
        if galdr::cable::needs_tuning() {
            return Self::Large;
        }
        Self::Fine
    }
}

impl GaldrApp {
    /// Below the microphone when it is VB-CABLE, or while there is something to report.
    fn cable_ui_shown(&self) -> bool {
        let vb_cable = self
            .settings
            .mic
            .as_deref()
            .and_then(|k| self.devices.mic(k))
            .is_some_and(|m| m.microphone.contains("VB-Audio Virtual Cable"));
        match self.cable {
            CableTune::Fine => false,
            CableTune::Large => vb_cable,
            _ => true,
        }
    }

    fn cable_ui(&mut self, ui: &mut egui::Ui, locked: bool) {
        let small = |text: &str| RichText::new(text).small();
        if let CableTune::Waiting(rx) = &self.cable {
            match rx.try_recv() {
                Ok(Ok(done)) => {
                    // The cable restarted: its devices are new to Windows.
                    self.devices = Devices::scan();
                    self.cable = CableTune::Done(done);
                }
                Ok(Err(e)) if e == "cancelled" => self.cable = CableTune::Large,
                Ok(Err(e)) => self.cable = CableTune::Failed(e),
                Err(mpsc::TryRecvError::Empty) => {
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.cable = CableTune::Failed("Could not change the VB-CABLE setting".into());
                }
            }
        }
        ui.horizontal_wrapped(|ui| match &self.cable {
            CableTune::Fine => {}
            CableTune::Large => {
                ui.label(small("VB-CABLE adds extra delay by default.").weak());
                let lower = ui
                    .add_enabled(!locked, egui::Button::new(small("Lower it")))
                    .on_hover_text("Asks Windows for administrator rights, then restarts VB-CABLE")
                    .on_disabled_hover_text("Disconnect first: VB-CABLE restarts");
                if lower.clicked() {
                    self.cable = CableTune::Waiting(lower_cable_latency());
                }
            }
            CableTune::Waiting(_) => {
                ui.label(small("Waiting for Windows…").weak());
            }
            CableTune::Done(text) => {
                ui.label(small(text).color(GREEN));
            }
            CableTune::Failed(text) => {
                ui.label(small(text).color(RED));
            }
        });
    }
}

/// Runs Galdr again as administrator to set VB-CABLE's buffer; the answer comes on the channel.
fn lower_cable_latency() -> mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        #[cfg(windows)]
        let result = match galdr::cable::tune_elevated(TUNE_CABLE_ARG) {
            Ok(galdr::cable::Tuned::Restarted) => Ok("Done: VB-CABLE delay lowered.".into()),
            Ok(galdr::cable::Tuned::RebootNeeded) => {
                Ok("Saved. Restart the PC to apply it.".into())
            }
            Err(e) => Err(e.to_string()),
        };
        #[cfg(not(windows))]
        let result = Err("Only on Windows".into());
        let _ = tx.send(result);
    });
    rx
}

/// "CABLE Output (VB-Audio Virtual Cable)" -> "VB-Audio Virtual Cable".
fn cable_name(device: &str) -> String {
    match (device.rfind('('), device.rfind(')')) {
        (Some(a), Some(b)) if a < b => device[a + 1..b].to_string(),
        _ => device.to_string(),
    }
}

impl GaldrApp {
    /// Tray status dot: dark when disconnected, green while speaking, grey otherwise.
    fn update_tray(&mut self) {
        let Some(tray) = &self.tray else { return };
        let look = match self.running() {
            None => TrayLook::Off,
            Some(s) if s.level >= SPEAKING_ON => TrayLook::Speaking,
            Some(s) if self.tray_look == TrayLook::Speaking && s.level > SPEAKING_OFF => {
                TrayLook::Speaking
            }
            Some(_) => TrayLook::Quiet,
        };
        if look != self.tray_look {
            self.tray_look = look;
            let _ = tray.set_icon(Some(tray_icon_image(look)));
            let _ = tray.set_tooltip(Some(match look {
                TrayLook::Off => "Galdr — not connected",
                TrayLook::Quiet => "Galdr — connected",
                TrayLook::Speaking => "Galdr — speaking",
            }));
        }
    }
}

impl eframe::App for GaldrApp {
    /// Runs before every frame and also while the window is hidden in the tray.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        #[cfg(windows)]
        hide_title_icon(frame);
        #[cfg(not(windows))]
        let _ = frame;
        if std::mem::take(&mut self.start_hidden) {
            // eframe shows the window after its first frame even when built invisible.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.update_tray();
        self.update_discovery(ctx);
        if self.engine.is_some() {
            ctx.request_repaint_after(REFRESH);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested())
            && self.settings.close_to_tray
            && self.tray.is_some()
            && !self.quitting.load(Ordering::Relaxed)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        if self.saved.as_ref() != Some(&self.settings) {
            self.settings.save();
            self.saved = Some(self.settings.clone());
        }

        let stats = self.running();
        if stats.is_none() {
            // Surface why a finished engine stopped, then drop it.
            if let Some(Stats {
                state: State::Failed(why),
                ..
            }) = self.engine.as_ref().map(Engine::stats)
            {
                self.error = Some(why);
            }
            self.engine = None;
        }

        let panel = egui::Frame::central_panel(ui.style()).inner_margin(18);
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            // Scrolls only if Advanced is open in a small window.
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("galdr").size(26.0).strong().color(ACCENT));
                    });
                    ui.add_space(12.0);
                    self.phones_ui(ui, stats.as_ref());
                    ui.add_space(12.0);
                    self.settings_ui(ui, stats.is_some());
                    ui.add_space(10.0);

                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut self.settings.connect_on_start, "Connect on startup");
                        ui.add_space(12.0);
                        let start = ui
                            .checkbox(&mut self.autostart, "Start with Windows")
                            .on_hover_text("Starts hidden in the tray");
                        if start.changed()
                            && let Err(e) = system::set_autostart(self.autostart)
                        {
                            self.autostart = system::autostart_enabled();
                            self.error = Some(format!("Start with Windows: {e}"));
                        }
                        ui.add_space(12.0);
                        ui.checkbox(&mut self.settings.close_to_tray, "Close to tray");
                    });
                    ui.add_space(4.0);
                    self.advanced_ui(ui, stats.is_some());
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Wait here so playback stops cleanly before the process ends.
        self.disconnect();
        self.finish_stopping();
    }
}

const ACCENT: Color32 = Color32::from_rgb(230, 160, 40);
const GREEN: Color32 = Color32::from_rgb(90, 190, 110);
const YELLOW: Color32 = Color32::from_rgb(220, 180, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 130, 50);
const RED: Color32 = Color32::from_rgb(220, 80, 80);

fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, Proportional)),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(15.0, Proportional)),
            (TextStyle::Small, FontId::new(12.5, Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(14.0, egui::FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.slider_width = 220.0;
        style.interaction.selectable_labels = false;
        // Text fields in the same grey as the dropdowns, not near-black.
        style.visuals.text_edit_bg_color = Some(style.visuals.widgets.inactive.weak_bg_fill);
    });
}

/// A button with a drawn circular arrow: font glyphs like "⟳" sit off-centre.
fn refresh_button(ui: &mut egui::Ui, width: f32) -> egui::Response {
    let height = ui.spacing().interact_size.y;
    let response = ui.add(egui::Button::new("").min_size(egui::vec2(width, height)));
    let color = ui.style().interact(&response).fg_stroke.color;
    let stroke = egui::Stroke::new(1.6, color);
    let c = response.rect.center();
    let r = 5.5;
    // Clockwise arc (angle decreasing) from 340° round to 30°: gap on the right,
    // arrowhead at the upper-right end, like "↻".
    let (start, end) = (340f32.to_radians(), 30f32.to_radians());
    let steps = 24;
    let points: Vec<egui::Pos2> = (0..=steps)
        .map(|i| {
            let a = start + (end - start) * i as f32 / steps as f32;
            c + egui::vec2(a.cos(), -a.sin()) * r
        })
        .collect();
    let tip = *points.last().unwrap();
    ui.painter().add(egui::Shape::line(points, stroke));
    // Arrowhead at the end of the arc, pointing along the clockwise direction.
    let dir = egui::vec2(end.sin(), end.cos());
    let side = egui::vec2(-dir.y, dir.x);
    let head = [tip + dir * 3.2, tip - side * 3.0, tip + side * 3.0];
    ui.painter().add(egui::Shape::convex_polygon(
        head.to_vec(),
        color,
        egui::Stroke::NONE,
    ));
    response
}

/// A titled, rounded block.
fn section<R>(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .inner_margin(12)
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(15.0));
            ui.add_space(4.0);
            body(ui)
        })
        .inner
}

fn accent_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .size(15.0)
            .strong()
            .color(Color32::BLACK),
    )
    .fill(ACCENT)
}

/// Options side by side in a thin pill, the chosen one filled: a choice, not more buttons.
/// Drawn by hand: a frame in a row would stretch to the row's full height.
fn segmented<T: PartialEq + Copy, const N: usize>(
    ui: &mut egui::Ui,
    // Total height, to line up with the button beside it.
    height: f32,
    value: &mut T,
    // Label, value and hover text (none when empty).
    options: [(&str, T, &str); N],
) {
    let pad_x = 11.0;
    let inset = 2.0;
    let galleys = options.map(|(label, _, _)| {
        egui::WidgetText::from(RichText::new(label).size(14.0)).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Button,
        )
    });
    let height = height - 2.0 * inset;
    let widths = galleys.each_ref().map(|g| g.size().x + 2.0 * pad_x);
    let size = egui::vec2(
        widths.iter().sum::<f32>() + 2.0 * inset,
        height + 2.0 * inset,
    );
    let (rect, pill) = ui.allocate_exact_size(size, egui::Sense::hover());
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        size.y / 2.0,
        visuals.widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let mut x = rect.left() + inset;
    for (i, ((_, option, hover), galley)) in options.into_iter().zip(galleys).enumerate() {
        let segment = egui::Rect::from_min_size(
            egui::pos2(x, rect.top() + inset),
            egui::vec2(widths[i], height),
        );
        x += widths[i];
        let mut response = ui.interact(segment, pill.id.with(i), egui::Sense::click());
        if !hover.is_empty() {
            response = response.on_hover_text(hover);
        }
        if response.clicked() {
            *value = option;
        }
        let chosen = *value == option;
        if chosen {
            painter.rect_filled(segment, height / 2.0, visuals.widgets.inactive.weak_bg_fill);
        }
        let color = if chosen {
            visuals.strong_text_color()
        } else if response.hovered() {
            visuals.text_color()
        } else {
            visuals.weak_text_color()
        };
        let pos = segment.center() - galley.size() / 2.0;
        painter.galley(pos, galley, color);
    }
}

fn status_line(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new("●").color(color));
        // Own column so long errors wrap under the text instead of running off the window.
        ui.vertical(|ui| ui.add(egui::Label::new(RichText::new(text).color(color)).wrap()));
    });
}

/// One phone as the user sees it: reachable over Wi-Fi, USB or both. The network and adb know
/// it by different addresses, so the two are matched by name and Android version.
struct PhoneCard {
    name: String,
    details: String,
    wifi: Option<Phone>,
    usb: Option<Phone>,
}

impl PhoneCard {
    fn group(phones: &[Phone]) -> Vec<PhoneCard> {
        let android = |details: &str| {
            details
                .split('·')
                .map(str::trim)
                .find(|part| part.starts_with("Android"))
                .map(str::to_string)
        };
        let mut cards: Vec<PhoneCard> = Vec::new();
        for phone in phones {
            let usb = phone.usb.is_some();
            let same = cards.iter_mut().find(|c| {
                let other = c.wifi.as_ref().or(c.usb.as_ref());
                let free = if usb {
                    c.usb.is_none()
                } else {
                    c.wifi.is_none()
                };
                free && c.name == phone.name
                    && other.and_then(|o| android(&o.details)) == android(&phone.details)
                    && android(&phone.details).is_some()
            });
            let card = match same {
                Some(card) => card,
                None => {
                    cards.push(PhoneCard {
                        name: phone.name.clone(),
                        details: String::new(),
                        wifi: None,
                        usb: None,
                    });
                    cards.last_mut().unwrap()
                }
            };
            // The network's details also name the model when the phone was renamed.
            if !usb || card.details.is_empty() {
                card.details = phone.details.clone();
            }
            if usb {
                card.usb = Some(phone.clone());
            } else {
                card.wifi = Some(phone.clone());
            }
        }
        cards
    }

    /// `key` (see `Phone::key`) is one of the ways to this phone.
    fn has(&self, key: &str) -> bool {
        [&self.wifi, &self.usb]
            .into_iter()
            .flatten()
            .any(|p| p.key() == key)
    }
}

fn status_ui(ui: &mut egui::Ui, s: &Stats, show_stats: bool) {
    let (color, text) = match &s.state {
        State::Connecting => (YELLOW, "Connecting…".to_string()),
        State::Streaming => (
            GREEN,
            format!(
                "Streaming → {}",
                s.outputs
                    .iter()
                    .map(|o| cable_name(o))
                    .collect::<Vec<_>>()
                    .join(" + ")
            ),
        ),
        State::Silent => (ORANGE, "Phone sends only silence".to_string()),
        State::Reconnecting(why) => (YELLOW, format!("Reconnecting: {why}")),
        State::Failed(why) => (RED, why.clone()),
        State::Stopped => (Color32::GRAY, "Stopped".to_string()),
    };
    status_line(ui, color, &text);
    ui.add_space(2.0);
    ui.add(
        egui::ProgressBar::new(s.level.sqrt()) // sqrt: speech is visible without shouting
            .desired_height(14.0)
            .corner_radius(4)
            .fill(if s.level > 0.9 { RED } else { GREEN }),
    );
    if show_stats {
        ui.label(
            RichText::new(format!(
                "packets {}   ·   lost {}   ·   bad {}   ·   buffer {} / {} ms",
                s.received, s.lost, s.bad, s.buffered_ms, s.target_ms
            ))
            .small()
            .weak(),
        );
    }
}

/// Tray icon with Show / Quit; a left click also brings the window back.
fn build_tray(ctx: &egui::Context, quitting: Arc<AtomicBool>) -> Option<TrayIcon> {
    let show = MenuItem::new("Show Galdr", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append(&show).ok()?;
    menu.append(&quit).ok()?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Galdr")
        .with_icon(tray_icon_image(TrayLook::Off))
        .build()
        .ok()?;

    let show_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            reveal(&show_ctx);
        }
    }));
    let menu_ctx = ctx.clone();
    let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            reveal(&menu_ctx);
        } else if event.id == quit_id {
            quitting.store(true, Ordering::Relaxed);
            menu_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            menu_ctx.request_repaint();
        }
    }));
    Some(tray)
}

fn reveal(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

const ICON_48: &[u8] = include_bytes!("../../assets/icon-48.png");

#[derive(Clone, Copy, PartialEq)]
enum TrayLook {
    Off,
    Quiet,
    Speaking,
}

/// A plain status dot: dark when disconnected, grey when connected, green while speaking.
fn tray_icon_image(look: TrayLook) -> tray_icon::Icon {
    const N: u32 = 32;
    let color = match look {
        TrayLook::Off => [90, 90, 90],
        TrayLook::Quiet => [170, 170, 170],
        TrayLook::Speaking => [70, 220, 90],
    };
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let alpha = (14.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[color[0], color[1], color[2], (alpha * 255.0) as u8]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, N, N).expect("valid icon")
}

fn main() -> eframe::Result {
    #[cfg(windows)]
    if std::env::args().any(|a| a == TUNE_CABLE_ARG) {
        std::process::exit(galdr::cable::exit_code(&galdr::cable::tune()));
    }
    if !system::claim_single_instance() {
        return Ok(()); // the running copy shows its window instead
    }
    system::dark_menus();
    let in_tray = std::env::args().any(|a| a == TRAY_ARG);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Galdr")
            .with_icon(eframe::icon_data::from_png_bytes(ICON_48).expect("bundled icon"))
            .with_inner_size([460.0, 470.0])
            .with_min_inner_size([430.0, 440.0])
            .with_visible(!in_tray),
        ..Default::default()
    };
    eframe::run_native(
        "galdr",
        options,
        Box::new(move |cc| Ok(Box::new(GaldrApp::new(cc, in_tray)))),
    )
}

/// One running copy, and starting with Windows.
#[cfg(windows)]
mod system {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use eframe::egui;
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
    };

    /// Named event: its existence marks a running copy, setting it asks that copy to show itself.
    const SHOW_EVENT: &str = "Local\\galdr-gui-show";
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const RUN_VALUE: &str = "Galdr";

    /// Dark popup menus (the tray menu) to match the always-dark window. Windows has no public
    /// API for this; uxtheme's unnamed exports 135 `SetPreferredAppMode` and 136
    /// `FlushMenuThemes` (Windows 10 1903+) are what Notepad++, Chromium and muda use.
    /// Older systems simply keep light menus.
    pub fn dark_menus() {
        const FORCE_DARK: i32 = 2;
        let dll = wide("uxtheme.dll");
        // SAFETY: the exports are looked up by ordinal and only called when present, with the
        // signatures they have had since 1903.
        unsafe {
            let uxtheme = LoadLibraryW(dll.as_ptr());
            if uxtheme.is_null() {
                return;
            }
            let ordinal = |n: usize| GetProcAddress(uxtheme, n as *const u8);
            if let (Some(set_mode), Some(flush)) = (ordinal(135), ordinal(136)) {
                let set_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
                let flush: extern "system" fn() = std::mem::transmute(flush);
                set_mode(FORCE_DARK);
                flush();
            }
        }
    }

    /// The event handle as an integer, so the watcher thread can own it.
    static EVENT: OnceLock<usize> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// True for the first copy. A second copy wakes the first one's window and gets false.
    pub fn claim_single_instance() -> bool {
        let name = wide(SHOW_EVENT);
        // SAFETY: plain Win32 calls with a NUL-terminated name; the handle is kept for the
        // process lifetime (first copy) or only used to signal (a later copy).
        unsafe {
            let event = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
            if event.is_null() {
                return true; // can't tell; better two windows than none
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                SetEvent(event);
                return false;
            }
            let _ = EVENT.set(event as usize);
        }
        true
    }

    /// Shows the window whenever another copy is started.
    pub fn on_show_request(ctx: egui::Context) {
        let Some(&event) = EVENT.get() else { return };
        let _ = std::thread::Builder::new()
            .name("galdr-show".into())
            .spawn(move || {
                loop {
                    // SAFETY: the handle stays open for the whole process.
                    unsafe { WaitForSingleObject(event as *mut c_void, INFINITE) };
                    super::reveal(&ctx);
                }
            });
    }

    pub fn autostart_enabled() -> bool {
        let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
        let mut size = 0u32;
        // SAFETY: a size query only (no buffer).
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        status == ERROR_SUCCESS
    }

    /// Adds or removes `"<this exe>" --tray` under the current user's Run key.
    pub fn set_autostart(on: bool) -> Result<(), String> {
        let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
        let status = if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let command = wide(&format!("\"{}\" {}", exe.display(), super::TRAY_ARG));
            // SAFETY: NUL-terminated strings; the data length is in bytes.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                )
            }
        } else if autostart_enabled() {
            // SAFETY: NUL-terminated strings.
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) }
        } else {
            ERROR_SUCCESS
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32).to_string())
        }
    }
}

#[cfg(not(windows))]
mod system {
    use eframe::egui;

    pub fn claim_single_instance() -> bool {
        true
    }
    pub fn on_show_request(_ctx: egui::Context) {}
    pub fn dark_menus() {}
    pub fn autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_on: bool) -> Result<(), String> {
        Err("not supported on this system".into())
    }
}

/// No icon in the title bar, but the window keeps it: the taskbar preview and Alt+Tab show it.
/// Only the caption is told not to draw it.
#[cfg(windows)]
fn hide_title_icon(frame: &eframe::Frame) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::AtomicBool;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Controls::{
        SetWindowThemeAttribute, WTA_NONCLIENT, WTA_OPTIONS, WTNCA_NODRAWICON, WTNCA_NOSYSMENU,
    };

    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return;
    };
    let hwnd = win.hwnd.get() as HWND;
    // NOSYSMENU only hides the icon's click menu; Alt+Space still opens it.
    let flags = WTNCA_NODRAWICON | WTNCA_NOSYSMENU;
    let options = WTA_OPTIONS {
        dwFlags: flags,
        dwMask: flags,
    };
    // SAFETY: a theme attribute on our own live window, with a correctly sized struct.
    let ok = unsafe {
        SetWindowThemeAttribute(
            hwnd,
            WTA_NONCLIENT,
            (&raw const options).cast(),
            size_of::<WTA_OPTIONS>() as u32,
        )
    };
    DONE.store(ok == 0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(name: &str, details: &str, usb: Option<&str>) -> Phone {
        Phone {
            name: name.into(),
            details: details.into(),
            addr: if usb.is_some() {
                Ipv4Addr::LOCALHOST.into()
            } else {
                Ipv4Addr::new(192, 168, 1, 20).into()
            },
            control_port: DEFAULT_CONTROL_PORT,
            usb: usb.map(str::to_string),
        }
    }

    #[test]
    fn one_card_per_phone_whichever_way_it_is_reached() {
        let cards = PhoneCard::group(&[
            phone("Pixel 7", "Android 17", None),
            phone("Pixel 7", "Android 17", Some("2A21")),
            phone("Pixel 7", "allow USB debugging on the phone", Some("R58N")),
            phone("Galaxy", "SM-S911B · Android 15", None),
        ]);
        let ways: Vec<(&str, bool, bool)> = cards
            .iter()
            .map(|c| (c.name.as_str(), c.wifi.is_some(), c.usb.is_some()))
            .collect();
        assert_eq!(
            ways,
            [
                ("Pixel 7", true, true),
                ("Pixel 7", false, true),
                ("Galaxy", true, false)
            ]
        );
        assert!(cards[0].has("usb:2A21") && cards[0].has("192.168.1.20"));
    }
}
