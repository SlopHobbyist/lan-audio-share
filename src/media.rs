//! Media keys, forwarded from the listening machine to the sending one.
//!
//! The machine you are sitting at is usually the one playing the audio, not the
//! one producing it, so its play/pause key is pointed at the wrong computer.
//! This takes the key away from the local machine and presses it on the far end
//! instead, which is what you meant by pressing it.
//!
//! Two halves, on opposite ends of the link:
//!
//! - [`capture`] runs on the receiver. It claims the media keys system-wide and
//!   hands each press to a callback, which puts it on the wire. Claiming them
//!   means the local media player stops seeing them, which is the point: one
//!   press should not pause two computers.
//! - [`press`] runs on the sender. It synthesises the key at the OS input layer,
//!   so whatever has the keys bound — Spotify, a browser, the system player —
//!   reacts exactly as if the key had been pressed on that keyboard.
//!
//! Both are off unless the user turns them on, on both machines. The sender side
//! in particular means accepting input from the network, so it is opt-in there
//! as well as here.

use crate::net::NetThreads;
use crate::protocol::MediaKey;
use crate::stats::Stats;
use anyhow::Result;
use std::net::UdpSocket;
use std::sync::Arc;

#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32 as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
use unsupported as imp;

/// Whether this platform can do any of it, for the UI to grey itself out.
pub const SUPPORTED: bool = imp::SUPPORTED;

/// A live grab on the machine's media keys. Dropping it gives them back.
pub struct Capture {
    _threads: NetThreads,
    refused: Vec<&'static str>,
}

impl Capture {
    /// Keys the OS would not hand over, usually because another program asked
    /// first. Worth telling the user about, but not worth failing over: the
    /// remaining keys still work.
    pub fn refused(&self) -> &[&'static str] {
        &self.refused
    }
}

/// Claim the media keys, calling `on_key` once per press.
///
/// `on_key` runs on a background thread owned by the returned handle, and is
/// expected to return promptly — on some platforms the OS stops delivering
/// events to a listener that takes too long.
pub fn capture(on_key: impl FnMut(MediaKey) + Send + 'static) -> Result<Capture> {
    let (threads, refused) = imp::capture(on_key)?;
    Ok(Capture {
        _threads: threads,
        refused,
    })
}

/// Press and release `key` on this machine, as though it came from its keyboard.
pub fn press(key: MediaKey) -> Result<()> {
    imp::press(key)
}

/// Claim this machine's media keys and send each press to whichever machine we
/// are playing audio from.
///
/// `socket` should be the one audio arrives on: the sender has heard from it
/// recently, which is what gets a datagram back through a firewall with no
/// inbound rule for this app.
pub fn forward(stats: Arc<Stats>, socket: UdpSocket) -> Result<Capture> {
    capture(move |key| {
        // No sender yet means there is nothing to control. The status panel
        // already says as much, so say nothing here.
        let Some(addr) = stats.sender_control_addr() else {
            return;
        };
        if socket.send_to(&key.encode_packet(), addr).is_ok() {
            stats.note_media_key(key);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The two halves against each other on this machine: claim the keys, press
    /// one, and check the claim caught it. This runs against the real OS input
    /// APIs rather than a stand-in, so it skips itself when the platform will not
    /// hand the keys over — another program holding them on Windows, or
    /// Accessibility not granted on macOS.
    ///
    /// Nothing else on the machine can see the press while the claim is held, so
    /// this cannot pause whatever is playing here.
    #[test]
    fn a_pressed_key_arrives_at_the_capture() {
        if !SUPPORTED {
            eprintln!("skipping: media keys are not implemented on this platform");
            return;
        }

        let (tx, rx) = mpsc::channel();
        let capture = match capture(move |key| {
            let _ = tx.send(key);
        }) {
            Ok(capture) => capture,
            Err(err) => {
                eprintln!("skipping: could not claim the media keys ({err})");
                return;
            }
        };

        let key = MediaKey::PlayPause;
        if capture.refused().contains(&key.label()) {
            eprintln!("skipping: another program owns the {} key", key.label());
            return;
        }

        press(key).expect("the key should be pressable");

        let seen = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the capture never saw the key that was pressed");
        assert_eq!(seen, key);
    }
}
