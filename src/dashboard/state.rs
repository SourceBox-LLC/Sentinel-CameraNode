//! `DashboardState` — the mutable state that lives behind the
//! [`super::handle::Dashboard`] mutex.
//!
//! Anything that needs to read or write fields on a running dashboard goes
//! through this struct. Methods here are lock-free (the caller in
//! `handle.rs` / `render.rs` / `commands.rs` already holds the mutex).

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Local;

use super::types::{CameraState, CameraStatus, LogEntry, LogLevel, SettingsInfo, View};
use crate::api::ApiClient;
use crate::storage::NodeDatabase;

pub struct DashboardState {
    pub node_id: String,
    pub api_url: String,
    /// Subscription plan of the owning org (advisory only — see the doc
    /// comment on `api::types::RegisterResponse::plan`). `None` when the
    /// backend hasn't reported a plan yet; the status-bar renderer hides
    /// the pill badge in that case.
    pub plan: Option<String>,
    /// `camera_id`s on this node that the backend has suspended by the
    /// plan cap. Populated by `Dashboard::set_disabled_cameras` on each
    /// heartbeat; consulted by the HLS uploader before every push to
    /// skip futile segment uploads, and by the camera-row renderer to
    /// show a `⚠ suspended` marker. Empty on the happy path.
    pub disabled_cameras: HashSet<String>,
    pub cameras: Vec<CameraState>,
    pub logs: VecDeque<LogEntry>,
    pub total_segments: u64,
    pub uptime_start: Instant,
    /// Maximum log lines to keep in memory.  `pub(super)` so
    /// [`super::handle::Dashboard::load_logs_from_db`] can read it.
    pub(super) log_capacity: usize,
    /// Current input bar text
    pub input_text: String,
    /// Cursor position within input text
    pub input_cursor: usize,
    /// Suppress debug logs until this instant (lets command output stay visible)
    pub suppress_debug_until: Option<Instant>,
    /// Persistent command output shown in a box above the input bar
    pub command_output: Vec<String>,
    /// Which view/screen is currently active
    pub current_view: View,
    /// Config info for the settings page
    pub settings: SettingsInfo,
    /// Database handle (for action commands like /wipe)
    pub db: Option<NodeDatabase>,
    /// HLS output directory (for cleanup on /wipe)
    pub hls_dir: Option<PathBuf>,
    /// API client (for /wipe backend decommission).  Optional because
    /// dashboards created in test-mode / `run_once` don't have one.
    pub api_client: Option<ApiClient>,
    /// Armed destructive-command confirmation.  Stores `(command, armed_at)`.
    /// Running the same command again without args within the timeout
    /// window (see [`CONFIRM_TIMEOUT`]) counts as confirmation.  Cleared
    /// as soon as *any* other command is dispatched, so the user can't
    /// accidentally confirm a pending wipe by typing /wipe minutes later
    /// after having done other things in between.
    pub pending_confirm: Option<(String, Instant)>,
}

/// How long after the first bare `/wipe` or `/reauth` a repeat press
/// still counts as confirmation.  30s is plenty of time to read the
/// warning and decide, but short enough that a forgotten terminal left
/// on the settings page won't accept a confirmation hours later.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

impl DashboardState {
    pub fn new(node_id: impl Into<String>, api_url: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            api_url: api_url.into(),
            plan: None,
            disabled_cameras: HashSet::new(),
            cameras: Vec::new(),
            logs: VecDeque::new(),
            total_segments: 0,
            uptime_start: Instant::now(),
            log_capacity: 200,
            input_text: String::new(),
            input_cursor: 0,
            suppress_debug_until: None,
            command_output: Vec::new(),
            current_view: View::Main,
            settings: SettingsInfo::default(),
            db: None,
            hls_dir: None,
            api_client: None,
            pending_confirm: None,
        }
    }

    /// In-memory part of a log event.  Pushes onto the ring buffer
    /// and returns a `(timestamp, level_str, body)` triple that the
    /// caller should pass to `NodeDatabase::save_log` **after**
    /// releasing the dashboard mutex.
    ///
    /// Why split: pre-v0.1.57 this method took the dashboard mutex
    /// *and* synchronously called `db.save_log` (which takes the
    /// SQLite mutex) while still holding it.  Every `Dashboard::log_*`
    /// caller — the render loop (2 ×/s), every spawned upload task
    /// (per-segment `is_camera_suspended` check + per-segment debug
    /// log), the heartbeat loop, the WS write-loop — would block on
    /// any other slow log call.  A WAL checkpoint or encrypted-row
    /// fsync pause would freeze the whole TUI for that duration.
    ///
    /// Splitting keeps the dashboard lock held for purely-in-memory
    /// work (push to `VecDeque`, prune to capacity).  Returns `None`
    /// when the log was suppressed (debug-level inside the
    /// post-command quiet window).
    #[must_use]
    pub fn log_inmem(
        &mut self,
        level: LogLevel,
        message: impl Into<String>,
    ) -> Option<(String, &'static str, String)> {
        // Suppress debug-level noise briefly after a command so output
        // stays visible.
        if matches!(level, LogLevel::Debug) {
            if let Some(until) = self.suppress_debug_until {
                if Instant::now() < until {
                    return None;
                }
                self.suppress_debug_until = None;
            }
        }
        let msg = message.into();

        let entry = LogEntry::new(level, msg.clone());
        self.logs.push_back(entry);
        while self.logs.len() > self.log_capacity {
            self.logs.pop_front();
        }

        // Hand the persist row back to the caller — they'll write it
        // outside the lock.  Only emit when a DB is wired up.
        if self.db.is_some() {
            let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string();
            let lvl = match level {
                LogLevel::Info => "INFO",
                LogLevel::Warn => "WARN",
                LogLevel::Error => "ERROR",
                LogLevel::Debug => "DEBUG",
            };
            Some((ts, lvl, msg))
        } else {
            None
        }
    }

    /// Legacy synchronous log path that writes to the DB while still
    /// holding the dashboard lock.  Kept for non-handle callers (none
    /// in-tree today) and existing tests; new code in `handle.rs`
    /// uses `log_inmem` + an outside-the-lock persist.
    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        if let Some((ts, lvl, body)) = self.log_inmem(level, message) {
            // DEBUG stays in-memory only — mirrors handle.rs::log_at.
            // Persisting per-segment debug chatter was constant WAL
            // traffic on the same mutex the archive hot path needs.
            if matches!(level, LogLevel::Debug) {
                return;
            }
            if let Some(ref db) = self.db {
                let _ = db.save_log(&ts, lvl, &body);
            }
        }
    }

    pub fn add_camera(&mut self, state: CameraState) {
        self.cameras.push(state);
    }

    pub fn update_camera_status(&mut self, name: &str, status: CameraStatus) {
        if let Some(cam) = self.cameras.iter_mut().find(|c| c.name == name) {
            cam.status = status;
        }
    }

    pub fn record_upload(&mut self, camera_name: &str, bytes: u64) {
        self.total_segments += 1;
        if let Some(cam) = self.cameras.iter_mut().find(|c| c.name == camera_name) {
            cam.segments_uploaded += 1;
            cam.bytes_uploaded += bytes;
        }
    }

    pub fn set_codec(&mut self, camera_name: &str, video: &str, audio: &str) {
        if let Some(cam) = self.cameras.iter_mut().find(|c| c.name == camera_name) {
            cam.video_codec = video.to_string();
            cam.audio_codec = audio.to_string();
        }
    }

    /// Human-readable uptime string for status-bar display.
    /// `pub(super)` so render.rs and the `/status` slash-command handler
    /// in commands.rs can both reach it without going through Dashboard.
    pub(super) fn uptime(&self) -> String {
        let secs = self.uptime_start.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{}h {}m {}s", h, m, s)
        } else if m > 0 {
            format!("{}m {}s", m, s)
        } else {
            format!("{}s", s)
        }
    }
}
