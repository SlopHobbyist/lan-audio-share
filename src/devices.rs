//! Audio device enumeration and selection.
//!
//! Capture itself is out of scope: on macOS a virtual loopback device (such as
//! Loopback Audio) already presents desktop audio as an ordinary input, and on
//! Windows cpal exposes WASAPI loopback devices the same way. So picking a
//! device is all that is needed on either end.

use crate::config::DeviceSel;
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, SampleFormat, SupportedStreamConfig};

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub id: Option<String>,
    pub name: String,
}

impl DeviceEntry {
    pub fn selection(&self) -> DeviceSel {
        DeviceSel {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
}

fn entry_for(device: &Device) -> Option<DeviceEntry> {
    let name = device.description().ok()?.name().to_string();
    let id = device.id().ok().map(|i| i.to_string());
    Some(DeviceEntry { id, name })
}

/// List devices usable in the given direction.
pub fn list(direction: Direction) -> Vec<DeviceEntry> {
    let host = cpal::default_host();
    let devices = match direction {
        Direction::Input => host.input_devices().map(|d| d.collect::<Vec<_>>()),
        Direction::Output => host.output_devices().map(|d| d.collect::<Vec<_>>()),
    };
    let mut out: Vec<DeviceEntry> = devices
        .unwrap_or_default()
        .iter()
        .filter_map(entry_for)
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.dedup_by(|a, b| a.id == b.id && a.name == b.name);
    out
}

/// The system default device for a direction, if there is one.
pub fn default_entry(direction: Direction) -> Option<DeviceEntry> {
    let host = cpal::default_host();
    let device = match direction {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }?;
    entry_for(&device)
}

/// Resolve a saved selection back to a real device.
///
/// The backend id is tried first, then the name. Names are what survive moving
/// the config between machines or re-plugging an interface, which is exactly
/// when the id stops matching.
pub fn resolve(direction: Direction, want: &DeviceSel) -> Result<Device> {
    let host = cpal::default_host();
    let devices: Vec<Device> = match direction {
        Direction::Input => host.input_devices().map(|d| d.collect()),
        Direction::Output => host.output_devices().map(|d| d.collect()),
    }
    .map_err(|e| anyhow!("could not enumerate audio devices: {e}"))?;

    if let Some(id) = want.id.as_deref() {
        for device in &devices {
            if device.id().ok().map(|i| i.to_string()).as_deref() == Some(id) {
                return Ok(device.clone());
            }
        }
    }

    for device in &devices {
        if device
            .description()
            .ok()
            .map(|d| d.name().to_string())
            .as_deref()
            == Some(&want.name)
        {
            return Ok(device.clone());
        }
    }

    Err(anyhow!("audio device \"{}\" is not available", want.name))
}

/// Pick a stream config for a device, preferring the requested sample rate and
/// channel count but falling back to something the device actually supports
/// rather than refusing to start.
pub fn choose_config(
    device: &Device,
    direction: Direction,
    wanted_rate: u32,
    wanted_channels: u16,
) -> Result<SupportedStreamConfig> {
    let ranges: Vec<_> = match direction {
        Direction::Input => device.supported_input_configs().map(|c| c.collect()),
        Direction::Output => device.supported_output_configs().map(|c| c.collect()),
    }
    .map_err(|e| anyhow!("could not query device capabilities: {e}"))?;

    // Prefer f32 and the exact channel count, then relax each in turn. Scoring
    // beats a pile of nested loops here and keeps the preference order visible.
    let score = |range: &cpal::SupportedStreamConfigRange| -> i32 {
        let mut score = 0;
        if range.contains_rate(wanted_rate) {
            score += 100;
        }
        if range.channels() == wanted_channels {
            score += 50;
        } else if range.channels() >= wanted_channels {
            score += 20;
        }
        score += match range.sample_format() {
            SampleFormat::F32 => 10,
            SampleFormat::I16 => 8,
            SampleFormat::I32 => 6,
            _ => 0,
        };
        score
    };

    let best = ranges
        .iter()
        .filter(|r| supported_format(r.sample_format()))
        .max_by_key(|r| score(r))
        .cloned();

    if let Some(best) = best {
        let rate = wanted_rate.clamp(best.min_sample_rate(), best.max_sample_rate());
        return Ok(best.with_sample_rate(rate));
    }

    // Nothing matched our format filter, so fall back to whatever the device
    // calls its default.
    let fallback = match direction {
        Direction::Input => device.default_input_config(),
        Direction::Output => device.default_output_config(),
    }
    .context("device reported no usable stream configuration")?;

    if !supported_format(fallback.sample_format()) {
        return Err(anyhow!(
            "device uses an unsupported sample format ({:?})",
            fallback.sample_format()
        ));
    }
    Ok(fallback)
}

/// Sample formats we know how to convert to and from f32.
pub fn supported_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32 | SampleFormat::U16
    )
}

pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not an assertion so much as a report: prints what this machine exposes,
    /// which is the first thing to check when a device will not open.
    #[test]
    fn report_devices() {
        for (label, direction) in [("input", Direction::Input), ("output", Direction::Output)] {
            let list = list(direction);
            println!("--- {label} devices ({}) ---", list.len());
            for entry in &list {
                println!("  {:?}  id={:?}", entry.name, entry.id);
            }
            println!("  default: {:?}", default_entry(direction).map(|e| e.name));
        }
    }
}
