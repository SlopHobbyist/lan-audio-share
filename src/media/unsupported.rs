//! Platforms with no media key support compiled in.
//!
//! The app targets Windows and macOS; this exists so that building anywhere else
//! fails at the feature, with something the user can read, rather than at the
//! compiler.

use crate::net::NetThreads;
use crate::protocol::MediaKey;
use anyhow::{Result, anyhow};

pub const SUPPORTED: bool = false;

fn unsupported() -> anyhow::Error {
    anyhow!("media key forwarding is only implemented on Windows and macOS")
}

pub fn press(_key: MediaKey) -> Result<()> {
    Err(unsupported())
}

pub fn prepare_press() -> Result<()> {
    Err(unsupported())
}

pub fn capture(
    _on_key: impl FnMut(MediaKey) + Send + 'static,
) -> Result<(NetThreads, Vec<&'static str>)> {
    Err(unsupported())
}
