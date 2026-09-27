//! Owns whichever role is currently running and keeps it running.
//!
//! Two things make the app feel like it "just works": settings are applied at
//! startup without asking, and a failure to start is not fatal. A virtual
//! loopback device may not be registered yet when the app launches at login, and
//! a USB interface may come and go, so a failed start turns into a quiet retry
//! rather than an error the user has to clear.

use crate::config::{Config, Mode};
use crate::receiver::{self, RecvRole};
use crate::sender::{self, SendRole};
use crate::stats::Stats;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How long to wait before retrying a role that failed to start.
const RETRY_DELAY: Duration = Duration::from_secs(2);

enum Role {
    Idle,
    Send(SendRole),
    Receive(RecvRole),
}

pub struct Engine {
    role: Role,
    /// The mode the running role corresponds to, so we can tell when the UI has
    /// asked for something different.
    running_mode: Mode,
    retry_at: Option<Instant>,
    pub stats: Arc<Stats>,
}

/// What the UI needs to describe the current state in words.
pub struct Status {
    pub active: bool,
    pub device_name: String,
    pub rate: u32,
    pub device_channels: u16,
    pub audio_port: Option<u16>,
    pub retrying: bool,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            role: Role::Idle,
            running_mode: Mode::Off,
            retry_at: None,
            stats: Arc::new(Stats::new()),
        }
    }

    /// Tear down whatever is running.
    pub fn stop(&mut self) {
        match &self.role {
            Role::Send(role) => role.pause(),
            Role::Receive(role) => role.pause(),
            Role::Idle => {}
        }
        self.role = Role::Idle;
        self.running_mode = Mode::Off;
        self.retry_at = None;
        self.stats.reset();
    }

    /// Start (or restart) the role described by `config`.
    pub fn apply(&mut self, config: &Config) {
        self.stop();
        if config.mode == Mode::Off {
            return;
        }

        let result = match config.mode {
            Mode::Send => sender::start(config, self.stats.clone()).map(Role::Send),
            Mode::Receive => receiver::start(config, self.stats.clone()).map(Role::Receive),
            Mode::Off => unreachable!(),
        };

        match result {
            Ok(role) => {
                self.role = role;
                self.running_mode = config.mode;
                self.stats.clear_error();
            }
            Err(err) => {
                // Report the root cause rather than the wrapper, which is what
                // actually tells the user what to fix.
                let root = err.root_cause().to_string();
                let message = if root == err.to_string() {
                    err.to_string()
                } else {
                    format!("{err}: {root}")
                };
                self.stats.set_error(message);
                self.role = Role::Idle;
                self.running_mode = config.mode;
                self.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
        }
    }

    /// Called every UI frame: restarts the role if the audio backend reported a
    /// problem, or if a previous start attempt failed and it is time to retry.
    pub fn tick(&mut self, config: &Config) {
        if config.mode == Mode::Off {
            return;
        }

        if self.stats.restart_requested.swap(false, Ordering::Relaxed) {
            self.retry_at = Some(Instant::now() + RETRY_DELAY);
            self.role = Role::Idle;
        }

        let needs_start = matches!(self.role, Role::Idle);
        if !needs_start {
            return;
        }

        match self.retry_at {
            Some(at) if Instant::now() < at => {}
            // `apply` reports its own outcome: it clears the error on success and
            // schedules the next retry on failure.
            _ => self.apply(config),
        }
    }

    pub fn status(&self) -> Status {
        match &self.role {
            Role::Send(role) => Status {
                active: true,
                device_name: role.device_name.clone(),
                rate: role.rate,
                device_channels: role.device_channels,
                audio_port: None,
                retrying: false,
            },
            Role::Receive(role) => Status {
                active: true,
                device_name: role.device_name.clone(),
                rate: role.rate,
                device_channels: role.device_channels,
                audio_port: Some(role.audio_port),
                retrying: false,
            },
            Role::Idle => Status {
                active: false,
                device_name: String::new(),
                rate: 0,
                device_channels: 0,
                audio_port: None,
                retrying: self.retry_at.is_some(),
            },
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
