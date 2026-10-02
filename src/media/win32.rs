//! Windows: `RegisterHotKey` to claim the media keys, `SendInput` to press them.
//!
//! A registered hotkey is owned exclusively for as long as it is registered, so
//! nothing else on this machine sees the press. That is the behaviour we want —
//! the press is meant for the other computer — and it comes for free, whereas a
//! low-level keyboard hook would have to decide whether to swallow each event
//! and answer within a timeout or be silently unhooked.
//!
//! The message loop polls rather than blocking in `GetMessageW`. A hotkey is a
//! rare event where 20 ms of added latency cannot be felt, and polling means the
//! thread stops on the same flag every other thread in this app stops on,
//! instead of needing a `WM_QUIT` posted into a queue that may not exist yet.

use crate::net::{self, NetThreads};
use crate::protocol::MediaKey;
use anyhow::{Result, anyhow};
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    MOD_NOREPEAT, RegisterHotKey, SendInput, UnregisterHotKey, VIRTUAL_KEY, VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, WM_HOTKEY,
};

pub const SUPPORTED: bool = true;

/// How long the hotkey thread sleeps between drains of its message queue.
const POLL: Duration = Duration::from_millis(20);

fn virtual_key(key: MediaKey) -> VIRTUAL_KEY {
    match key {
        MediaKey::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaKey::Next => VK_MEDIA_NEXT_TRACK,
        MediaKey::Previous => VK_MEDIA_PREV_TRACK,
        MediaKey::Stop => VK_MEDIA_STOP,
    }
}

/// Hotkey ids are scoped to the registering thread, so any distinct small
/// numbers will do. Deriving them from the wire code keeps the mapping in one
/// place and reversible.
fn hotkey_id(key: MediaKey) -> i32 {
    key.code() as i32 + 1
}

fn key_for_id(id: i32) -> Option<MediaKey> {
    u8::try_from(id - 1).ok().and_then(MediaKey::from_code)
}

fn keyboard_input(vk: VIRTUAL_KEY, release: bool) -> INPUT {
    // The media keys are extended keys on a real keyboard, so say so here too.
    let mut flags = KEYEVENTF_EXTENDEDKEY;
    if release {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// `SendInput` needs no permission.
pub fn prepare_press() -> Result<()> {
    Ok(())
}

pub fn press(key: MediaKey) -> Result<()> {
    let vk = virtual_key(key);
    let events = [keyboard_input(vk, false), keyboard_input(vk, true)];
    // SAFETY: `events` is a live array of exactly the count and size declared.
    let sent = unsafe { SendInput(2, events.as_ptr(), size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        return Err(anyhow!(
            "Windows accepted {sent} of {} key events (something is blocking \
             synthetic input, such as a game running as administrator)",
            events.len()
        ));
    }
    Ok(())
}

pub fn capture(
    mut on_key: impl FnMut(MediaKey) + Send + 'static,
) -> Result<(NetThreads, Vec<&'static str>)> {
    // Registration has to happen on the thread that reads the messages, so the
    // outcome comes back over a channel rather than as a return value.
    let (tx, rx) = mpsc::channel::<Result<Vec<&'static str>, String>>();

    let threads = net::spawn_stoppable("media-keys", move |stop| {
        let mut msg = MSG::default();
        // Give this thread a message queue before anything can be posted to it.
        // SAFETY: a null window handle asks for this thread's own messages.
        unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE) };

        let mut taken = Vec::new();
        let mut refused = Vec::new();
        for key in MediaKey::ALL {
            // SAFETY: a null window handle posts WM_HOTKEY to this thread's
            // message queue, which is what the loop below reads.
            let ok = unsafe {
                RegisterHotKey(
                    ptr::null_mut(),
                    hotkey_id(key),
                    MOD_NOREPEAT,
                    virtual_key(key) as u32,
                )
            };
            if ok != 0 {
                taken.push(key);
            } else {
                refused.push(key.label());
            }
        }

        if taken.is_empty() {
            let _ = tx.send(Err(
                "another program already owns the media keys on this machine".to_string(),
            ));
            return;
        }
        let _ = tx.send(Ok(refused));

        while !stop.load(Ordering::Relaxed) {
            // SAFETY: as above; `msg` is written only by this call.
            while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                if msg.message != WM_HOTKEY {
                    continue;
                }
                // wParam carries back the id we registered the key under.
                if let Some(key) = key_for_id(msg.wParam as i32) {
                    on_key(key);
                }
            }
            std::thread::sleep(POLL);
        }

        for key in taken {
            // SAFETY: these are ids this thread registered and still holds.
            unsafe { UnregisterHotKey(ptr::null_mut(), hotkey_id(key)) };
        }
    })?;

    // The thread registers immediately; a timeout here means it never ran.
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(refused)) => Ok((threads, refused)),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(_) => Err(anyhow!("the media key thread did not start")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id a key is registered under has to map back to that same key, since
    /// that round trip is how a press is identified.
    #[test]
    fn hotkey_ids_are_reversible() {
        for key in MediaKey::ALL {
            assert_eq!(key_for_id(hotkey_id(key)), Some(key), "{key:?}");
        }
        assert_eq!(key_for_id(0), None, "ids start at one");
    }

    /// A press has to be a down followed by an up of the same key, or whatever
    /// is listening sees a key held forever.
    #[test]
    fn builds_a_down_then_up_pair() {
        let vk = virtual_key(MediaKey::PlayPause);
        let down = keyboard_input(vk, false);
        let up = keyboard_input(vk, true);

        assert_eq!(down.r#type, INPUT_KEYBOARD);
        // SAFETY: both were built as keyboard events above.
        unsafe {
            assert_eq!(down.Anonymous.ki.wVk, vk);
            assert_eq!(up.Anonymous.ki.wVk, vk);
            assert_eq!(down.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP, 0);
            assert_ne!(up.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP, 0);
        }
    }
}
