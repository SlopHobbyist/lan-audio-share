//! State shared between the audio callbacks, the network threads and the UI.
//!
//! The audio callback must never block, so everything it touches is either an
//! atomic or behind a lock the callback only ever tries (never waits on).

use crate::protocol::MediaKey;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A receiver that has announced itself to us.
#[derive(Clone, Debug)]
pub struct Peer {
    pub addr: SocketAddr,
    pub name: String,
    pub last_seen: Instant,
    /// True when this peer came from the manual IP list rather than discovery.
    pub manual: bool,
}

impl Peer {
    pub fn is_live(&self, timeout: Duration) -> bool {
        self.manual || self.last_seen.elapsed() < timeout
    }
}

/// Everything the UI wants to display, plus the handful of values the network
/// thread needs to hand to the audio callback.
pub struct Stats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    /// Frames the receiver had to conceal because packets never arrived.
    pub lost_frames: AtomicU64,
    /// Packets discarded for arriving after their slot had already played.
    pub late_packets: AtomicU64,
    /// Times the receive buffer ran dry.
    pub underruns: AtomicU64,
    /// Current receive buffer depth in ms, as f32 bits.
    buffer_ms: AtomicU32,
    /// Applied clock-drift correction, in parts per million.
    pub drift_ppm: AtomicI32,
    /// Recent peak level (0.0..1.0) as f32 bits, for the level meter.
    peak: AtomicU32,
    /// Sample rate of the incoming stream, or 0 if nothing is arriving.
    pub source_rate: AtomicU32,
    /// Channel count of the incoming stream.
    pub source_channels: AtomicU32,
    /// Set by the network thread when the far end restarts, cleared by the audio
    /// callback once it has flushed the stale audio.
    pub resync: AtomicBool,
    /// Milliseconds since the last audio packet arrived, tracked as a coarse
    /// counter the UI can compare against.
    pub last_audio_at: Mutex<Option<Instant>>,
    pub peers: Mutex<Vec<Peer>>,
    /// Name the far end reports, for display.
    pub remote_name: Mutex<String>,
    /// When a sender last announced itself to us, so the UI can tell "no sender
    /// on the network" apart from "a sender is there but its audio is not".
    remote_seen_at: Mutex<Option<Instant>>,
    /// Where that sender's announcement came from, which is also where a media
    /// key has to be sent for it to arrive.
    remote_addr: Mutex<Option<SocketAddr>>,
    /// Address audio is actually arriving from. A fallback for addressing the
    /// sender when its announcements are not reaching us but its audio is.
    audio_source: Mutex<Option<IpAddr>>,
    /// Media keys forwarded (on a receiver) or pressed (on a sender), and the
    /// last one, so the UI can show the feature working.
    pub media_keys: AtomicU64,
    media_last: Mutex<Option<&'static str>>,
    /// Anything the user needs to know about media key forwarding. Separate from
    /// `error` because it must never disturb the audio, which keeps working.
    media_note: Mutex<Option<String>>,
    /// Last error worth showing the user.
    pub error: Mutex<Option<String>>,
    /// Set by a cpal error callback to ask the UI thread to rebuild the stream.
    pub restart_requested: AtomicBool,
    /// Output gain as f32 bits. Lives here so the slider takes effect instantly
    /// instead of having to rebuild the stream.
    volume: AtomicU32,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            lost_frames: AtomicU64::new(0),
            late_packets: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            buffer_ms: AtomicU32::new(0),
            drift_ppm: AtomicI32::new(0),
            peak: AtomicU32::new(0),
            source_rate: AtomicU32::new(0),
            source_channels: AtomicU32::new(0),
            resync: AtomicBool::new(false),
            last_audio_at: Mutex::new(None),
            peers: Mutex::new(Vec::new()),
            remote_name: Mutex::new(String::new()),
            remote_seen_at: Mutex::new(None),
            remote_addr: Mutex::new(None),
            audio_source: Mutex::new(None),
            media_keys: AtomicU64::new(0),
            media_last: Mutex::new(None),
            media_note: Mutex::new(None),
            error: Mutex::new(None),
            restart_requested: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    /// Clear the per-session counters, leaving configuration-ish fields alone.
    pub fn reset(&self) {
        self.packets.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.lost_frames.store(0, Ordering::Relaxed);
        self.late_packets.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        self.buffer_ms.store(0, Ordering::Relaxed);
        self.drift_ppm.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        self.source_rate.store(0, Ordering::Relaxed);
        self.source_channels.store(0, Ordering::Relaxed);
        self.resync.store(false, Ordering::Relaxed);
        self.restart_requested.store(false, Ordering::Relaxed);
        if let Ok(mut g) = self.last_audio_at.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.peers.lock() {
            g.clear();
        }
        if let Ok(mut g) = self.remote_name.lock() {
            g.clear();
        }
        if let Ok(mut g) = self.remote_seen_at.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.remote_addr.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.audio_source.lock() {
            *g = None;
        }
        self.media_keys.store(0, Ordering::Relaxed);
        if let Ok(mut g) = self.media_last.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.media_note.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.error.lock() {
            *g = None;
        }
    }

    pub fn set_volume(&self, gain: f32) {
        self.volume.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_buffer_ms(&self, ms: f32) {
        self.buffer_ms.store(ms.to_bits(), Ordering::Relaxed);
    }

    pub fn get_buffer_ms(&self) -> f32 {
        f32::from_bits(self.buffer_ms.load(Ordering::Relaxed))
    }

    /// Store a new peak, decaying the previous one so the meter falls smoothly.
    pub fn report_peak(&self, level: f32) {
        let prev = f32::from_bits(self.peak.load(Ordering::Relaxed));
        let decayed = prev * 0.80;
        self.peak
            .store(level.max(decayed).to_bits(), Ordering::Relaxed);
    }

    pub fn get_peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    pub fn note_audio(&self) {
        if let Ok(mut g) = self.last_audio_at.try_lock() {
            *g = Some(Instant::now());
        }
    }

    /// True when audio arrived recently enough to call the link up.
    pub fn audio_flowing(&self) -> bool {
        self.last_audio_at
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    }

    pub fn set_error(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.error.lock() {
            *g = Some(msg.into());
        }
    }

    pub fn clear_error(&self) {
        if let Ok(mut g) = self.error.lock() {
            *g = None;
        }
    }

    /// A copy of the current error, if any. Does not clear it: the UI redraws
    /// many times per second and needs the message to persist.
    pub fn error_message(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }

    /// Live peers, newest announcements included, with dead ones filtered out.
    pub fn live_peers(&self) -> Vec<Peer> {
        let timeout = Duration::from_millis(crate::protocol::PEER_TIMEOUT_MS);
        self.peers
            .lock()
            .map(|g| g.iter().filter(|p| p.is_live(timeout)).cloned().collect())
            .unwrap_or_default()
    }

    pub fn remote(&self) -> String {
        self.remote_name
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Record a sender's announcement, and where it came from.
    pub fn note_remote(&self, name: &str, addr: SocketAddr) {
        if let Ok(mut g) = self.remote_seen_at.lock() {
            *g = Some(Instant::now());
        }
        if let Ok(mut g) = self.remote_addr.lock() {
            *g = Some(addr);
        }
        if !name.is_empty()
            && let Ok(mut g) = self.remote_name.lock()
            && *g != name
        {
            *g = name.to_string();
        }
    }

    /// The sender currently announcing itself on the network, if any.
    pub fn announced_remote(&self) -> Option<String> {
        let timeout = Duration::from_millis(crate::protocol::PEER_TIMEOUT_MS);
        let recent = self
            .remote_seen_at
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|t| t.elapsed() < timeout);
        recent.then(|| self.remote())
    }

    /// Note the address audio is arriving from.
    pub fn note_audio_source(&self, ip: IpAddr) {
        if let Ok(mut g) = self.audio_source.lock() {
            *g = Some(ip);
        }
    }

    /// Where to send a control message so that it reaches the machine doing the
    /// sending.
    ///
    /// The address its announcements come from is the exact answer. Failing that
    /// — networks exist where beacons get through in one direction only — the
    /// address its audio comes from, paired with the well-known discovery port it
    /// is always listening on, gets there too.
    pub fn sender_control_addr(&self) -> Option<SocketAddr> {
        if self.announced_remote().is_some()
            && let Some(addr) = self.remote_addr.lock().ok().and_then(|g| *g)
        {
            return Some(addr);
        }
        if !self.audio_flowing() {
            return None;
        }
        let ip = self.audio_source.lock().ok().and_then(|g| *g)?;
        Some(SocketAddr::from((ip, crate::protocol::DISCOVERY_PORT)))
    }

    /// Count a media key that was forwarded or pressed.
    pub fn note_media_key(&self, key: MediaKey) {
        self.media_keys.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = self.media_last.lock() {
            *g = Some(key.label());
        }
    }

    /// How many media keys have moved, and which was last, for the UI.
    pub fn media_activity(&self) -> (u64, Option<&'static str>) {
        (
            self.media_keys.load(Ordering::Relaxed),
            self.media_last.lock().ok().and_then(|g| *g),
        )
    }

    pub fn set_media_note(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.media_note.lock() {
            *g = Some(msg.into());
        }
    }

    pub fn media_note(&self) -> Option<String> {
        self.media_note.lock().ok().and_then(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DISCOVERY_PORT;

    /// A media key has nowhere to go until a sender has been heard from.
    #[test]
    fn no_sender_means_no_control_address() {
        let stats = Stats::new();
        assert_eq!(stats.sender_control_addr(), None);
    }

    /// An announcement is the exact answer: it says which port to reply on.
    #[test]
    fn prefers_the_address_a_sender_announced_from() {
        let stats = Stats::new();
        let announced = SocketAddr::from(([192, 168, 1, 5], DISCOVERY_PORT));
        stats.note_remote("studio-mac", announced);
        assert_eq!(stats.sender_control_addr(), Some(announced));
    }

    /// Audio arriving is enough on its own: some networks pass beacons in one
    /// direction only, and the sender is always listening on the discovery port.
    #[test]
    fn falls_back_to_where_the_audio_comes_from() {
        let stats = Stats::new();
        stats.note_audio_source(IpAddr::from([192, 168, 1, 7]));
        assert_eq!(
            stats.sender_control_addr(),
            None,
            "a stale source address must not be used"
        );

        stats.note_audio();
        assert_eq!(
            stats.sender_control_addr(),
            Some(SocketAddr::from(([192, 168, 1, 7], DISCOVERY_PORT)))
        );
    }

    /// Restarting a role has to forget the far end, or keys would be sent to a
    /// machine that is no longer there.
    #[test]
    fn reset_forgets_the_control_address() {
        let stats = Stats::new();
        stats.note_remote("studio-mac", SocketAddr::from(([192, 168, 1, 5], 47_771)));
        stats.note_media_key(MediaKey::Next);
        assert!(stats.sender_control_addr().is_some());

        stats.reset();
        assert_eq!(stats.sender_control_addr(), None);
        assert_eq!(stats.media_activity(), (0, None));
    }
}
