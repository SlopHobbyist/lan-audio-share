//! Settings that persist between runs.
//!
//! The whole point of this app is that launching it resumes exactly what it was
//! doing last time, so the config is saved the moment anything changes and is
//! applied unconditionally at startup.

use crate::protocol::{DEFAULT_AUDIO_PORT, WireFormat};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Mode {
    Off,
    Send,
    Receive,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "OFF",
            Mode::Send => "SEND",
            Mode::Receive => "RECEIVE",
        }
    }
}

/// How a device is remembered across runs.
///
/// The backend id is the reliable key, but it is not portable between machines
/// and can change when a device is re-plugged, so the human-readable name is
/// kept as a fallback match.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct DeviceSel {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mode: Mode,
    pub send_device: Option<DeviceSel>,
    pub recv_device: Option<DeviceSel>,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    /// Target depth of the receive jitter buffer, in milliseconds. This is a
    /// floor: the receiver raises its own target if it underruns.
    pub jitter_ms: u32,
    pub format: WireFormat,
    pub audio_port: u16,
    /// Optional comma-separated IPs to unicast to when discovery cannot work
    /// (multicast blocked, different subnet). Empty means rely on discovery.
    pub manual_peers: String,
    pub volume: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Off,
            send_device: None,
            recv_device: None,
            sample_rate: 48_000,
            buffer_frames: 2048,
            // A 2048-frame capture buffer means audio is produced in ~43 ms
            // chunks, so the receive buffer has to be at least that deep to
            // ride out one chunk. This adapts upward on its own if needed.
            jitter_ms: 60,
            format: WireFormat::F32,
            audio_port: DEFAULT_AUDIO_PORT,
            manual_peers: String::new(),
            volume: 1.0,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "LanAudioShare")?;
        Some(dirs.config_dir().join("config.json"))
    }

    /// Load saved settings, falling back to defaults for anything unreadable so
    /// a corrupt or partial file can never stop the app from starting.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        // Write-then-rename so an interrupted save cannot leave a truncated file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Channels are fixed at stereo: this exists to keep the intent obvious at
    /// the call sites that care.
    pub fn channels(&self) -> u16 {
        2
    }

    /// Manual peer list, parsed. Entries without a port get the default one.
    pub fn parsed_manual_peers(&self) -> Vec<std::net::SocketAddr> {
        use std::net::{SocketAddr, ToSocketAddrs};
        let mut out = Vec::new();
        for raw in self
            .manual_peers
            .split([',', ';', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(addr) = raw.parse::<SocketAddr>() {
                out.push(addr);
                continue;
            }
            let with_port = format!("{raw}:{}", DEFAULT_AUDIO_PORT);
            if let Ok(mut addrs) = with_port.to_socket_addrs()
                && let Some(addr) = addrs.find(|a| a.is_ipv4())
            {
                out.push(addr);
            }
        }
        out
    }
}
