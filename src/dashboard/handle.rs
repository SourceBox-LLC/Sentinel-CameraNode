//! `Dashboard` — the public, cheap-to-clone handle most of the codebase
//! imports.
//!
//! Wraps `Arc<Mutex<DashboardState>>` and provides the lifecycle / setup
//! methods (`new`, `log_*`, `set_db`, `set_disabled_cameras`, etc.).
//! Rendering and slash-command dispatch live in sibling modules
//! (`super::render`, `super::commands`); they add their own `impl`
//! blocks to this same struct.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::state::DashboardState;
use super::types::{CameraState, CameraStatus, LogEntry, LogLevel, SettingsInfo};
use crate::api::ApiClient;
use crate::storage::NodeDatabase;

/// Thread-safe handle to the dashboard state.
#[derive(Clone)]
pub struct Dashboard(pub Arc<Mutex<DashboardState>>);

impl Dashboard {
    pub fn new(node_id: impl Into<String>, api_url: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(DashboardState::new(node_id, api_url))))
    }

    pub fn log_info(&self, msg: impl Into<String>) {
        self.log_at(LogLevel::Info, msg);
    }

    pub fn log_warn(&self, msg: impl Into<String>) {
        self.log_at(LogLevel::Warn, msg);
    }

    pub fn log_error(&self, msg: impl Into<String>) {
        self.log_at(LogLevel::Error, msg);
    }

    /// Shared body for `log_info`/`log_warn`/`log_error` (and the
    /// `log_debug` variant added below).  Acquires the dashboard lock
    /// only for the in-memory push, then persists to the SQLite log
    /// table **outside** the lock.  See the
    /// `DashboardState::log_inmem` doc for the rationale —
    /// short version: avoid blocking the render loop and every
    /// per-segment upload task on a slow WAL checkpoint.
    fn log_at(&self, level: LogLevel, msg: impl Into<String>) {
        let (to_persist, db_handle) = match self.0.lock() {
            Ok(mut s) => {
                let p = s.log_inmem(level, msg);
                // Clone is cheap — NodeDatabase is Arc<Mutex<…>>.
                (p, s.db.clone())
            }
            Err(_) => return,
        };
        // Lock is dropped here.  The SQLite write below can take its
        // own mutex / wait on disk without blocking the dashboard.
        //
        // DEBUG stays in-memory only (the TUI ring still shows it).
        // Persisting it meant per-segment chatter ("Segment N pushed",
        // once per second per camera, forever) became 1-2 SQLite
        // INSERTs/s/cam of WAL traffic + DB-mutex acquisitions
        // interleaved with the archive hot path — ~690K rows/day at
        // 8 cams, pruned right back to 10K every 5 minutes.  Pure
        // write amplification for a status line `record_upload`
        // already surfaces.
        if matches!(level, LogLevel::Debug) {
            return;
        }
        if let (Some((ts, lvl, body)), Some(db)) = (to_persist, db_handle) {
            let _ = db.save_log(&ts, lvl, &body);
        }
    }

    pub fn set_settings(&self, info: SettingsInfo) {
        if let Ok(mut s) = self.0.lock() {
            s.settings = info;
        }
    }

    /// Replace the node_id displayed in the TUI status bar (and
    /// returned by `/api/status`).  Defensive: the registration
    /// response is authoritative for `node_id` (the backend can
    /// canonicalize aliases — case-fold, legacy mappings), so the
    /// value coming back may differ from what we sent.  Called from
    /// `run_internal` right after `register_with_cloud` returns so
    /// the dashboard reflects the backend's view even when the two
    /// differ.  Connected first-boot is gated on a persisted
    /// node_id by `main.rs::needs_setup`, so the rarely-relevant
    /// "before registration" case shouldn't actually be reachable
    /// in production — this setter is for the canonicalize-differs
    /// case and for tests.
    pub fn set_node_id(&self, node_id: impl Into<String>) {
        if let Ok(mut s) = self.0.lock() {
            s.node_id = node_id.into();
        }
    }

    /// Record the org's subscription plan for display in the status bar.
    ///
    /// Empty-string / whitespace-only values are treated as `None` so the
    /// backend can unset the badge by sending `""` without us rendering an
    /// empty pill. Purely informational — the node does not enforce any
    /// plan-based limits.
    pub fn set_plan(&self, plan: Option<String>) {
        if let Ok(mut s) = self.0.lock() {
            s.plan = plan
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
        }
    }

    /// Replace the set of camera_ids suspended by the plan cap, as
    /// reported by the backend in the heartbeat response.  Logs a
    /// transition line for every camera that newly went suspended or
    /// newly returned — called once per heartbeat, so steady-state is
    /// silent but a plan downgrade / upgrade shows up in the log.
    pub fn set_disabled_cameras(&self, camera_ids: Vec<String>) {
        // Build and diff outside the lock-holding block so we don't hold
        // the state mutex across `log_warn` / `log_info`, which also lock.
        let incoming: HashSet<String> = camera_ids.into_iter().collect();

        let (newly_suspended, newly_resumed): (Vec<String>, Vec<String>) =
            if let Ok(mut s) = self.0.lock() {
                let newly_suspended: Vec<String> = incoming
                    .difference(&s.disabled_cameras)
                    .cloned()
                    .collect();
                let newly_resumed: Vec<String> = s
                    .disabled_cameras
                    .difference(&incoming)
                    .cloned()
                    .collect();
                s.disabled_cameras = incoming;
                (newly_suspended, newly_resumed)
            } else {
                return;
            };

        for cam_id in &newly_suspended {
            self.log_warn(format!(
                "Camera {} suspended by plan cap — upload paused until upgrade",
                cam_id
            ));
        }
        for cam_id in &newly_resumed {
            self.log_info(format!(
                "Camera {} resumed — plan cap cleared",
                cam_id
            ));
        }
    }

    /// Whether the given camera_id is currently suspended by the backend's
    /// plan cap.  Called by the HLS uploader before every push to skip
    /// segments that the backend would 402.  Takes a short-lived read lock;
    /// callers should not hold the result across await points.
    pub fn is_camera_suspended(&self, camera_id: &str) -> bool {
        self.0
            .lock()
            .map(|s| s.disabled_cameras.contains(camera_id))
            .unwrap_or(false)
    }

    pub fn set_db(&self, db: NodeDatabase, hls_dir: PathBuf) {
        if let Ok(mut s) = self.0.lock() {
            s.db = Some(db);
            s.hls_dir = Some(hls_dir);
        }
    }

    /// Inject the API client so `/wipe confirm` can call the backend's
    /// `POST /api/nodes/self/decommission` before scrubbing local state.
    /// Optional — if no client is set, `/wipe` falls back to the
    /// local-only behaviour (data wiped, backend entry left as stale).
    pub fn set_api_client(&self, api_client: ApiClient) {
        if let Ok(mut s) = self.0.lock() {
            s.api_client = Some(api_client);
        }
    }

    /// Pre-populate the TUI log buffer with entries from the database so logs
    /// survive restarts.
    pub fn load_logs_from_db(&self) {
        if let Ok(mut s) = self.0.lock() {
            let db = match s.db {
                Some(ref db) => db.clone(),
                None => return,
            };
            let rows = match db.load_recent_logs(s.log_capacity) {
                Ok(r) => r,
                Err(_) => return,
            };
            for (timestamp, level_str, message) in rows {
                let level = match level_str.as_str() {
                    "WARN"  => LogLevel::Warn,
                    "ERROR" => LogLevel::Error,
                    "DEBUG" => LogLevel::Debug,
                    _       => LogLevel::Info,
                };
                // Use the stored timestamp's time portion for display
                let time = if timestamp.len() >= 19 {
                    timestamp[11..19].to_string()
                } else {
                    timestamp.clone()
                };
                s.logs.push_back(LogEntry { time, level, message });
            }
        }
    }

    pub fn log_debug(&self, msg: impl Into<String>) {
        self.log_at(LogLevel::Debug, msg);
    }

    pub fn add_camera(&self, state: CameraState) {
        if let Ok(mut s) = self.0.lock() {
            s.add_camera(state);
        }
    }

    pub fn update_camera_status(&self, name: &str, status: CameraStatus) {
        if let Ok(mut s) = self.0.lock() {
            s.update_camera_status(name, status);
        }
    }

    /// Attach the cloud-assigned `camera_id` to a dashboard row (matched
    /// by display name). Called once, right after cloud registration,
    /// so downstream per-ID lookups can find this camera.
    pub fn set_camera_id(&self, name: &str, camera_id: &str) {
        if let Ok(mut s) = self.0.lock() {
            if let Some(cam) = s.cameras.iter_mut().find(|c| c.name == name) {
                cam.camera_id = camera_id.to_string();
            }
        }
    }

    /// Update a camera's pipeline status by its cloud-side camera_id.
    /// Used by the supervisor loop where we have the camera_id on hand
    /// but not necessarily the display name.
    pub fn update_camera_status_by_id(&self, camera_id: &str, status: CameraStatus) {
        if let Ok(mut s) = self.0.lock() {
            if let Some(cam) = s.cameras.iter_mut().find(|c| c.camera_id == camera_id) {
                cam.status = status;
            }
        }
    }

    /// Look up a camera's current status by cloud-side camera_id. Returns
    /// `None` if no camera with that id is registered. Heartbeat builders
    /// call this so they can send the real pipeline state instead of the
    /// old hardcoded "streaming".
    pub fn get_camera_status_by_id(&self, camera_id: &str) -> Option<CameraStatus> {
        let s = self.0.lock().ok()?;
        s.cameras
            .iter()
            .find(|c| c.camera_id == camera_id)
            .map(|c| c.status.clone())
    }

    pub fn record_upload(&self, camera_name: &str, bytes: u64) {
        if let Ok(mut s) = self.0.lock() {
            s.record_upload(camera_name, bytes);
        }
    }

    pub fn set_codec(&self, camera_name: &str, video: &str, audio: &str) {
        if let Ok(mut s) = self.0.lock() {
            s.set_codec(camera_name, video, audio);
        }
    }
}
