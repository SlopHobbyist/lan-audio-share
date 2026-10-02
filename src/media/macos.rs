//! macOS: a `CGEventTap` to claim the media keys, a synthetic `NSSystemDefined`
//! event to press them.
//!
//! Media keys are not ordinary keystrokes on this platform. They arrive as
//! system-defined events with the key packed into `data1`, which is why neither
//! half of this can use the normal keyboard APIs: there is no virtual key code
//! for play/pause to send.
//!
//! Capturing needs permission. The tap is an active one so that it can swallow
//! the press — one key should not pause this machine as well as the far one —
//! and an active tap is Accessibility-gated. `CGEventTapCreate` simply returns
//! null when that has not been granted, so the error path says where to grant it.
//!
//! Pressing needs no permission: posting to the HID tap is how every media
//! remote on the platform does it.

use crate::net::{self, NetThreads};
use crate::protocol::MediaKey;
use anyhow::{Result, anyhow};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CGPoint, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGPreflightListenEventAccess, CGRequestListenEventAccess,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

pub const SUPPORTED: bool = true;

/// `NX_SYSDEFINED`, the event type the media keys arrive as. Not in the
/// `CGEventType` list because AppKit, not CoreGraphics, defines what it means.
const NX_SYSDEFINED: u32 = 14;

/// The subtype carried by an auxiliary (media) key event.
const SUBTYPE_AUX_KEY: i16 = 8;

/// `NX_KEYTYPE_*` codes, as packed into the high half of `data1`.
const NX_KEYTYPE_PLAY: i64 = 16;
const NX_KEYTYPE_NEXT: i64 = 17;
const NX_KEYTYPE_PREVIOUS: i64 = 18;
const NX_KEYTYPE_FAST: i64 = 19;
const NX_KEYTYPE_REWIND: i64 = 20;

/// Key state, as packed into the second byte of `data1`.
const KEY_DOWN: i64 = 0xA;
const KEY_UP: i64 = 0xB;

/// How long the tap thread waits in its run loop before re-checking the stop
/// flag.
const RUN_SLICE: f64 = 0.25;

/// The code a key is *sent* as.
///
/// Next and previous go out as fast-forward and rewind because that is what the
/// keys on an Apple keyboard emit, and emitting what the hardware emits is the
/// whole promise of this feature. There is no system-defined stop key.
fn send_code(key: MediaKey) -> Option<i64> {
    match key {
        MediaKey::PlayPause => Some(NX_KEYTYPE_PLAY),
        MediaKey::Next => Some(NX_KEYTYPE_FAST),
        MediaKey::Previous => Some(NX_KEYTYPE_REWIND),
        MediaKey::Stop => None,
    }
}

/// The key a received code means. Both spellings of next and previous are
/// accepted, since third-party keyboards with dedicated track buttons send the
/// `NEXT`/`PREVIOUS` pair rather than the Apple `FAST`/`REWIND` pair.
fn received_key(code: i64) -> Option<MediaKey> {
    match code {
        NX_KEYTYPE_PLAY => Some(MediaKey::PlayPause),
        NX_KEYTYPE_FAST | NX_KEYTYPE_NEXT => Some(MediaKey::Next),
        NX_KEYTYPE_REWIND | NX_KEYTYPE_PREVIOUS => Some(MediaKey::Previous),
        _ => None,
    }
}

fn post(code: i64, state: i64) -> Result<()> {
    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::SystemDefined,
        // `NSPoint` is CoreGraphics' `CGPoint` under another name.
        CGPoint::ZERO,
        // What a real media key carries. Not a modifier in any useful sense.
        NSEventModifierFlags(0xa00),
        0.0,
        0,
        // Documented as unused, and required to be nil.
        None,
        SUBTYPE_AUX_KEY,
        (code << 16) | (state << 8),
        -1,
    )
    .ok_or_else(|| anyhow!("could not build a system-defined key event"))?;

    let cg = event
        .CGEvent()
        .ok_or_else(|| anyhow!("the key event had no CoreGraphics form"))?;
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&cg));
    Ok(())
}

pub fn press(key: MediaKey) -> Result<()> {
    let code = send_code(key).ok_or_else(|| {
        anyhow!(
            "macOS has no {} key to press, so that one cannot be forwarded here",
            key.label()
        )
    })?;
    post(code, KEY_DOWN)?;
    post(code, KEY_UP)
}

/// What the tap callback needs, kept in one allocation behind the `user_info`
/// pointer the OS hands back to us.
struct TapState {
    on_key: Box<dyn FnMut(MediaKey) + Send>,
    /// Filled in once the tap exists, so the callback can switch it back on if
    /// the system disables it.
    port: Option<CFRetained<CFMachPort>>,
}

/// # Safety
///
/// `user_info` must be the `TapState` pointer handed to `CGEventTapCreate`, and
/// this must run on the thread that created the tap, so the exclusive borrow
/// taken here cannot alias.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let state = unsafe { &mut *(user_info as *mut TapState) };

    // A tap that was too slow, or that the user interrupted, is switched off and
    // stays off until asked to resume. Without this the feature dies silently.
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        if let Some(port) = state.port.as_deref() {
            CGEvent::tap_enable(port, true);
        }
        return event.as_ptr();
    }

    if kind.0 != NX_SYSDEFINED {
        return event.as_ptr();
    }

    // SAFETY: the OS guarantees `event` is a valid CGEvent for this call.
    let Some(ns) = NSEvent::eventWithCGEvent(unsafe { event.as_ref() }) else {
        return event.as_ptr();
    };
    if ns.subtype().0 != SUBTYPE_AUX_KEY {
        return event.as_ptr();
    }

    let data1 = ns.data1();
    let Some(key) = received_key((data1 >> 16) & 0xffff) else {
        return event.as_ptr();
    };

    // Act on the press, and swallow both halves so the local player never sees
    // the key — this press was meant for the other machine.
    if (data1 >> 8) & 0xff == KEY_DOWN {
        (state.on_key)(key);
    }
    std::ptr::null_mut()
}

pub fn capture(
    on_key: impl FnMut(MediaKey) + Send + 'static,
) -> Result<(NetThreads, Vec<&'static str>)> {
    // The tap must be created on the thread that runs its run loop, so the
    // outcome comes back over a channel rather than as a return value.
    let (tx, rx) = mpsc::channel::<Result<(), String>>();

    let threads = net::spawn_stoppable("media-keys", move |stop| {
        let state = Box::into_raw(Box::new(TapState {
            on_key: Box::new(on_key),
            port: None,
        }));
        run_tap(stop, state, &tx);
        // SAFETY: `state` came from `Box::into_raw` just above and nothing else
        // owns it. `run_tap` invalidates the tap before returning, so the
        // callback cannot fire again.
        drop(unsafe { Box::from_raw(state) });
    })?;

    match rx.recv_timeout(Duration::from_secs(2)) {
        // No key can be refused individually here: the tap is all or nothing.
        Ok(Ok(())) => Ok((threads, Vec::new())),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(_) => Err(anyhow!("the media key thread did not start")),
    }
}

/// Build the tap, then pump its run loop until asked to stop.
fn run_tap(stop: &AtomicBool, state: *mut TapState, tx: &mpsc::Sender<Result<(), String>>) {
    // Asking first is what produces the system prompt; without it the user only
    // ever sees the failure.
    if !CGPreflightListenEventAccess() {
        CGRequestListenEventAccess();
    }

    // SAFETY: the callback matches the required signature, and `state` is a
    // live, uniquely-owned `TapState` for as long as this function runs.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            (1 as CGEventMask) << NX_SYSDEFINED,
            Some(tap_callback),
            state as *mut c_void,
        )
    };
    let Some(port) = port else {
        let _ = tx.send(Err("macOS would not allow the media keys to be captured. \
             Allow LAN Audio Share under System Settings > Privacy & Security > \
             Accessibility, then switch this back on."
            .to_string()));
        return;
    };

    // SAFETY: nothing else references `state`, and the callback cannot run until
    // the run loop below starts.
    unsafe { (*state).port = Some(port.clone()) };

    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = tx.send(Err(
            "could not attach the media key tap to a run loop".to_string()
        ));
        port.invalidate();
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = tx.send(Err("the media key thread has no run loop".to_string()));
        port.invalidate();
        return;
    };

    // SAFETY: reading mode constants exported by CoreFoundation.
    let common_modes = unsafe { kCFRunLoopCommonModes };
    let default_mode = unsafe { kCFRunLoopDefaultMode };

    run_loop.add_source(Some(&source), common_modes);
    CGEvent::tap_enable(&port, true);
    let _ = tx.send(Ok(()));

    while !stop.load(Ordering::Relaxed) {
        CFRunLoop::run_in_mode(default_mode, RUN_SLICE, false);
    }

    // Hand the keys back before the thread and its run loop go away.
    CGEvent::tap_enable(&port, false);
    run_loop.remove_source(Some(&source), common_modes);
    source.invalidate();
    port.invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever a key is sent as has to be understood as that same key coming
    /// back, or two Macs would not agree with each other.
    #[test]
    fn sent_codes_read_back_as_the_same_key() {
        for key in MediaKey::ALL {
            let Some(code) = send_code(key) else { continue };
            assert_eq!(received_key(code), Some(key), "{key:?}");
        }
    }

    /// The pair a third-party keyboard sends has to mean the same thing as the
    /// pair an Apple keyboard sends.
    #[test]
    fn accepts_both_spellings_of_next_and_previous() {
        assert_eq!(received_key(NX_KEYTYPE_NEXT), Some(MediaKey::Next));
        assert_eq!(received_key(NX_KEYTYPE_FAST), Some(MediaKey::Next));
        assert_eq!(received_key(NX_KEYTYPE_PREVIOUS), Some(MediaKey::Previous));
        assert_eq!(received_key(NX_KEYTYPE_REWIND), Some(MediaKey::Previous));
        // Brightness, volume and the rest must not be mistaken for transport.
        assert_eq!(received_key(7), None);
        assert_eq!(received_key(0), None);
    }

    /// Stop has no macOS key, and must say so rather than press the wrong thing.
    #[test]
    fn stop_is_reported_as_unsupported() {
        assert!(send_code(MediaKey::Stop).is_none());
        let err = press(MediaKey::Stop).expect_err("stop should not be pressable");
        assert!(err.to_string().contains("stop"), "{err}");
    }
}
