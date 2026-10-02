//! The single window.
//!
//! Two rules shape this UI: nothing needs to be touched for audio to flow after
//! a launch, and every change is saved the instant it is made. There is no
//! connect button, no mute, and no apply step.

use crate::config::{Config, Mode};
use crate::devices::{self, DeviceEntry, Direction};
use crate::engine::Engine;
use crate::media;
use crate::protocol::WireFormat;
use eframe::egui;
use std::time::{Duration, Instant};

/// How often the device list is re-enumerated. Enumeration is not free, so it
/// happens on a timer rather than every frame.
const DEVICE_REFRESH: Duration = Duration::from_secs(5);

const SAMPLE_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
const BUFFER_SIZES: [u32; 6] = [256, 512, 1024, 2048, 4096, 8192];

/// What forwarding media keys does, which depends on which end of the link this
/// machine is.
fn media_help(mode: Mode) -> &'static str {
    if !media::SUPPORTED {
        return "Not available on this platform.";
    }
    match mode {
        Mode::Receive => {
            "Play/pause, next, previous and stop are taken from this machine and \
             pressed on the sender instead. Nothing here will see them."
        }
        Mode::Send => {
            "Presses media keys sent by the machine listening to this one, as \
             though they came from this keyboard."
        }
        Mode::Off => {
            "Presses the listening machine's media keys on the sending machine \
             instead. Has to be on at both ends."
        }
    }
}

pub struct App {
    config: Config,
    engine: Engine,
    input_devices: Vec<DeviceEntry>,
    output_devices: Vec<DeviceEntry>,
    devices_refreshed: Instant,
    /// Set when a setting changed that requires rebuilding the audio stream.
    /// Applied once the user stops interacting, so dragging a slider does not
    /// restart the stream on every pixel.
    restart_pending: bool,
    save_pending: bool,
}

impl App {
    pub fn new() -> Self {
        let config = Config::load();
        let mut engine = Engine::new();
        engine.stats.set_volume(config.volume);
        // Resume immediately: this is the whole point of the app.
        engine.apply(&config);

        Self {
            config,
            engine,
            input_devices: devices::list(Direction::Input),
            output_devices: devices::list(Direction::Output),
            devices_refreshed: Instant::now(),
            restart_pending: false,
            save_pending: false,
        }
    }

    fn refresh_devices(&mut self) {
        if self.devices_refreshed.elapsed() < DEVICE_REFRESH {
            return;
        }
        self.devices_refreshed = Instant::now();
        self.input_devices = devices::list(Direction::Input);
        self.output_devices = devices::list(Direction::Output);
    }

    fn mode_selector(&mut self, ui: &mut egui::Ui) {
        let mut wanted = self.config.mode;
        let width = (ui.available_width() - 16.0) / 3.0;

        ui.horizontal(|ui| {
            for mode in [Mode::Off, Mode::Send, Mode::Receive] {
                let selected = self.config.mode == mode;
                let label = egui::RichText::new(mode.label()).size(15.0).strong();
                if ui
                    .add_sized([width, 38.0], egui::Button::selectable(selected, label))
                    .clicked()
                {
                    wanted = mode;
                }
            }
        });

        if wanted != self.config.mode {
            self.config.mode = wanted;
            self.engine.apply(&self.config);
            self.config.save();
        }
    }

    fn device_picker(&mut self, ui: &mut egui::Ui) {
        let direction = match self.config.mode {
            Mode::Send => Direction::Input,
            Mode::Receive => Direction::Output,
            Mode::Off => return,
        };

        let (list, current, heading) = match direction {
            Direction::Input => (
                self.input_devices.clone(),
                self.config.send_device.clone(),
                "Send from",
            ),
            Direction::Output => (
                self.output_devices.clone(),
                self.config.recv_device.clone(),
                "Listen on",
            ),
        };

        let selected_text = current
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "System default".to_string());

        ui.label(egui::RichText::new(heading).strong());

        let mut chosen: Option<Option<DeviceEntry>> = None;
        egui::ComboBox::from_id_salt("device-picker")
            .selected_text(selected_text)
            .width(ui.available_width() - 8.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.is_none(), "System default")
                    .clicked()
                {
                    chosen = Some(None);
                }
                for entry in &list {
                    let is_current = current.as_ref().is_some_and(|c| {
                        c.name == entry.name || (c.id.is_some() && c.id == entry.id)
                    });
                    if ui.selectable_label(is_current, &entry.name).clicked() {
                        chosen = Some(Some(entry.clone()));
                    }
                }
                if list.is_empty() {
                    ui.label(egui::RichText::new("no devices found").weak().italics());
                }
            });

        if let Some(choice) = chosen {
            let selection = choice.map(|e| e.selection());
            match direction {
                Direction::Input => self.config.send_device = selection,
                Direction::Output => self.config.recv_device = selection,
            }
            self.engine.apply(&self.config);
            self.config.save();
        }

        if direction == Direction::Input {
            ui.label(
                egui::RichText::new(
                    "Pick a loopback device (for example Loopback Audio) to stream desktop sound.",
                )
                .weak()
                .size(11.0),
            );
        }
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let stats = self.engine.stats.clone();
        let status = self.engine.status();

        if let Some(error) = stats.error_message() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("⚠").color(egui::Color32::from_rgb(220, 140, 60)));
                ui.label(egui::RichText::new(error).color(egui::Color32::from_rgb(220, 140, 60)));
            });
            if status.retrying {
                ui.label(egui::RichText::new("Retrying…").weak().size(11.0));
            }
            ui.add_space(4.0);
        }

        // Media keys failing is worth saying, but it is not an audio problem and
        // must not read like one: the stream below is still fine.
        if let Some(note) = stats.media_note() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("⌨").color(egui::Color32::from_rgb(220, 140, 60)));
                ui.label(
                    egui::RichText::new(note)
                        .color(egui::Color32::from_rgb(220, 140, 60))
                        .size(11.0),
                );
            });
            ui.add_space(4.0);
        }

        if self.config.mode == Mode::Off {
            ui.label(egui::RichText::new("Idle. Choose SEND or RECEIVE to start.").weak());
            return;
        }

        if !status.active {
            return;
        }

        let flowing = stats.audio_flowing();

        // One clear line saying whether this is working.
        let (dot, headline, colour) = match self.config.mode {
            Mode::Send => {
                let peers = stats.live_peers();
                if peers.is_empty() {
                    (
                        "○",
                        "Waiting for a listener on the LAN…".to_string(),
                        egui::Color32::GRAY,
                    )
                } else {
                    let names: Vec<String> = peers
                        .iter()
                        .map(|p| {
                            if p.name.is_empty() {
                                p.addr.ip().to_string()
                            } else {
                                format!("{} ({})", p.name, p.addr.ip())
                            }
                        })
                        .collect();
                    (
                        "●",
                        format!("Streaming to {}", names.join(", ")),
                        egui::Color32::from_rgb(90, 190, 120),
                    )
                }
            }
            Mode::Receive => {
                if flowing {
                    let remote = stats.remote();
                    let who = if remote.is_empty() {
                        "the sender".to_string()
                    } else {
                        remote
                    };
                    (
                        "●",
                        format!("Playing audio from {who}"),
                        egui::Color32::from_rgb(90, 190, 120),
                    )
                } else if let Some(sender) = stats.announced_remote() {
                    // Discovery works but audio does not: that narrows it down to
                    // the sender's input or a firewall on this machine.
                    let who = if sender.is_empty() {
                        "A sender".to_string()
                    } else {
                        sender
                    };
                    (
                        "○",
                        format!(
                            "{who} is on the network, but no audio is arriving. Check \
                             it has a listener, and that this app is allowed through \
                             the firewall here."
                        ),
                        egui::Color32::from_rgb(220, 140, 60),
                    )
                } else {
                    (
                        "○",
                        "Listening. Waiting for a sender…".to_string(),
                        egui::Color32::GRAY,
                    )
                }
            }
            Mode::Off => unreachable!(),
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(dot).color(colour).size(15.0));
            ui.label(egui::RichText::new(headline).color(colour));
        });

        ui.add_space(6.0);

        // Level meter: immediate proof that audio is moving.
        let peak = stats.get_peak().clamp(0.0, 1.0);
        ui.add(
            egui::ProgressBar::new(peak)
                .desired_width(ui.available_width() - 8.0)
                .text(if peak > 0.0001 {
                    format!("{:.0} dB", 20.0 * peak.log10())
                } else {
                    "silent".to_string()
                }),
        );

        // A silent input streams perfectly and plays nothing, which looks
        // exactly like a network fault from the other end.
        if self.config.mode == Mode::Send && peak <= 0.0001 {
            ui.label(
                egui::RichText::new(
                    "The input is silent. Check audio is actually playing into this \
                     device (and, on macOS, that Microphone access is allowed).",
                )
                .color(egui::Color32::from_rgb(220, 140, 60))
                .size(11.0),
            );
        }

        ui.add_space(6.0);

        egui::Grid::new("stats-grid")
            .num_columns(2)
            .spacing([12.0, 3.0])
            .show(ui, |ui| {
                let mut row = |label: &str, value: String| {
                    ui.label(egui::RichText::new(label).weak().size(11.0));
                    ui.label(egui::RichText::new(value).size(11.0).monospace());
                    ui.end_row();
                };

                row(
                    "device",
                    format!(
                        "{} · {} Hz · {} ch",
                        status.device_name, status.rate, status.device_channels
                    ),
                );

                if self.config.mode == Mode::Receive {
                    let source_rate = stats.source_rate.load(std::sync::atomic::Ordering::Relaxed);
                    row("buffer", format!("{:.0} ms", stats.get_buffer_ms()));
                    row(
                        "clock drift",
                        format!(
                            "{:+} ppm",
                            stats.drift_ppm.load(std::sync::atomic::Ordering::Relaxed)
                        ),
                    );
                    if source_rate != 0 && source_rate != status.rate {
                        row(
                            "resampling",
                            format!("{source_rate} Hz → {} Hz", status.rate),
                        );
                    }
                    row(
                        "glitches",
                        format!(
                            "{} underruns · {} lost frames · {} late",
                            stats.underruns.load(std::sync::atomic::Ordering::Relaxed),
                            stats.lost_frames.load(std::sync::atomic::Ordering::Relaxed),
                            stats
                                .late_packets
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                    );
                    if let Some(port) = status.audio_port {
                        row("port", port.to_string());
                    }
                } else {
                    row(
                        "bitrate",
                        format!(
                            "{} kbit/s · {}",
                            crate::sender::bitrate_kbps(status.rate, self.config.format),
                            self.config.format.label()
                        ),
                    );
                }

                if self.config.media_keys {
                    let (count, last) = stats.media_activity();
                    let verb = if self.config.mode == Mode::Send {
                        "pressed here"
                    } else {
                        "sent"
                    };
                    row(
                        "media keys",
                        match last {
                            Some(key) => format!("{count} {verb} · last {key}"),
                            None => format!("none {verb} yet"),
                        },
                    );
                }

                row(
                    "packets",
                    format!(
                        "{} · {:.1} MB",
                        stats.packets.load(std::sync::atomic::Ordering::Relaxed),
                        stats.bytes.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_048_576.0
                    ),
                );
            });
    }

    fn advanced(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Advanced")
            .default_open(false)
            .show(ui, |ui| {
                let mut restart = false;

                egui::Grid::new("advanced-grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Sample rate");
                        egui::ComboBox::from_id_salt("rate")
                            .selected_text(format!("{} Hz", self.config.sample_rate))
                            .show_ui(ui, |ui| {
                                for rate in SAMPLE_RATES {
                                    if ui
                                        .selectable_label(
                                            self.config.sample_rate == rate,
                                            format!("{rate} Hz"),
                                        )
                                        .clicked()
                                    {
                                        self.config.sample_rate = rate;
                                        restart = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Buffer size");
                        egui::ComboBox::from_id_salt("buffer")
                            .selected_text(format!("{} frames", self.config.buffer_frames))
                            .show_ui(ui, |ui| {
                                for size in BUFFER_SIZES {
                                    let ms = size as f64 * 1000.0 / self.config.sample_rate as f64;
                                    if ui
                                        .selectable_label(
                                            self.config.buffer_frames == size,
                                            format!("{size} frames ({ms:.0} ms)"),
                                        )
                                        .clicked()
                                    {
                                        self.config.buffer_frames = size;
                                        restart = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Wire format");
                        egui::ComboBox::from_id_salt("format")
                            .selected_text(self.config.format.label())
                            .show_ui(ui, |ui| {
                                for format in [WireFormat::F32, WireFormat::I16] {
                                    if ui
                                        .selectable_label(
                                            self.config.format == format,
                                            format.label(),
                                        )
                                        .clicked()
                                    {
                                        self.config.format = format;
                                        restart = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Receive buffer");
                        let response = ui.add(
                            egui::Slider::new(&mut self.config.jitter_ms, 10..=300).suffix(" ms"),
                        );
                        if response.changed() {
                            self.restart_pending = true;
                            self.save_pending = true;
                        }
                        ui.end_row();

                        ui.label("Volume");
                        let response = ui.add(
                            egui::Slider::new(&mut self.config.volume, 0.0..=2.0)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                        );
                        if response.changed() {
                            // Gain is read live by the audio callback, so this
                            // needs no restart.
                            self.engine.stats.set_volume(self.config.volume);
                            self.save_pending = true;
                        }
                        ui.end_row();

                        ui.label("Audio port");
                        let mut port = self.config.audio_port.to_string();
                        if ui
                            .add(egui::TextEdit::singleline(&mut port).desired_width(80.0))
                            .changed()
                            && let Ok(value) = port.trim().parse::<u16>()
                            && value >= 1024
                        {
                            self.config.audio_port = value;
                            self.restart_pending = true;
                            self.save_pending = true;
                        }
                        ui.end_row();
                    });

                ui.add_space(4.0);
                let mut media_keys = self.config.media_keys;
                ui.add_enabled_ui(media::SUPPORTED, |ui| {
                    if ui.checkbox(&mut media_keys, "Forward media keys").changed() {
                        self.config.media_keys = media_keys;
                        restart = true;
                    }
                });
                ui.label(
                    egui::RichText::new(media_help(self.config.mode))
                        .weak()
                        .size(10.0),
                );

                ui.add_space(4.0);
                ui.label(egui::RichText::new("Send directly to (optional)").size(11.0));
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.manual_peers)
                            .hint_text("192.168.1.20")
                            .desired_width(ui.available_width() - 8.0),
                    )
                    .changed()
                {
                    self.restart_pending = true;
                    self.save_pending = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Only needed if the two machines cannot discover each other \
                         (multicast blocked, or different subnets).",
                    )
                    .weak()
                    .size(10.0),
                );

                ui.add_space(6.0);
                if let Some(path) = Config::path() {
                    ui.label(
                        egui::RichText::new(format!("settings: {}", path.display()))
                            .weak()
                            .size(10.0),
                    );
                }

                if restart {
                    self.restart_pending = true;
                    self.save_pending = true;
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh_devices();
        self.engine.tick(&self.config);

        egui::Frame::central_panel(ui.style())
            .inner_margin(14)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;

                self.mode_selector(ui);
                ui.add_space(2.0);
                self.device_picker(ui);
                ui.separator();
                self.status_panel(ui);
                ui.separator();
                self.advanced(ui);
            });

        // Apply deferred changes once the user has let go, so a drag does not
        // rebuild the audio stream on every frame.
        if !ui.ctx().egui_is_using_pointer() {
            if self.restart_pending {
                self.restart_pending = false;
                self.engine.apply(&self.config);
            }
            if self.save_pending {
                self.save_pending = false;
                self.config.save();
            }
        }

        // Keep the meters and counters live without spinning the CPU.
        ui.ctx().request_repaint_after(Duration::from_millis(80));
    }

    fn on_exit(&mut self) {
        self.config.save();
        self.engine.stop();
    }
}
