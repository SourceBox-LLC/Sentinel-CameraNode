// SourceBox Sentry CloudNode - Camera streaming node for SourceBox Sentry Cloud
// Copyright (C) 2026  SourceBox LLC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//! Node Runner
//!
//! Main orchestration: camera detection, registration, streaming, and the
//! live dashboard that replaces raw log output while the node is running.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use colored::Colorize;
use crate::config::Config;
use crate::error::Result;
use crate::camera::{self, DetectedCamera};
use crate::api::ApiClient;
use crate::storage::NodeDatabase;
// HttpServer is referenced via crate::server::HttpServer in
// run_internal — no top-level import needed since the new_with_api
// constructor is the only call site.
use crate::streaming::{HlsGeneratorConfig, HlsUploader, HlsUploaderConfig};
use crate::streaming::supervisor::{supervise_hls, PipelineSource, SupervisorConfig};
use crate::dashboard::{CameraState, CameraStatus, Dashboard};

/// Main node orchestrator
pub struct Node {
    config: Config,
    api_client: ApiClient,
    hls_output_dir: PathBuf,
    db: NodeDatabase,
}

/// Handles for the per-camera tasks started at boot. The supervisor owns
/// the FFmpeg child inside its task — unlike the pre-supervisor design
/// there is no `HlsGenerator` to drop here.
struct RunningStream {
    supervisor_handle: tokio::task::JoinHandle<()>,
    upload_handle: tokio::task::JoinHandle<()>,
}

impl Node {
    pub async fn new(config: Config) -> Result<Self> {
        // Local-mode nodes never talk to a Command Center — give them a
        // stub client so the rest of the codebase keeps the same shape
        // (Dashboard's `/wipe` flow + the uploader both hold an
        // `ApiClient`) without `Option<ApiClient>` plumbing.  Connected
        // mode builds the real reqwest-backed client with the configured
        // URL + API key.
        let api_client = if config.mode.is_local() {
            ApiClient::local_stub()?
        } else {
            ApiClient::new(&config.cloud.api_url, &config.cloud.api_key)?
        };
        // `paths::data_dir()` is the canonical absolute resolver for
        // where node.db / HLS segments / recordings live: env var
        // (`SOURCEBOX_SENTRY_DATA_DIR`) → platform default
        // (`%ProgramData%\SourceBoxSentry` on Windows, `./data` on
        // Linux/macOS where cwd is predictable).  No
        // `config.storage.path` field anymore — see StorageConfig
        // docs for the v0.1.39 bug that field caused.
        let storage_path = crate::paths::data_dir();
        std::fs::create_dir_all(&storage_path)?;
        let db = NodeDatabase::new(&storage_path.join("node.db"))?;
        let hls_output_dir = storage_path.join("hls");
        std::fs::create_dir_all(&hls_output_dir)?;

        Ok(Self {
            config,
            api_client,
            db,
            hls_output_dir,
        })
    }

    fn build_settings_info(&self) -> crate::dashboard::SettingsInfo {
        // Compute the local URL the operator should open in a browser.
        // bind=0.0.0.0 expands to the resolved LAN IP (more useful on
        // the status bar than literal "0.0.0.0"); other binds are
        // shown as-is.
        let local_url = {
            let host = if self.config.server.bind == "0.0.0.0" {
                get_local_ip().unwrap_or_else(|| "0.0.0.0".to_string())
            } else {
                self.config.server.bind.clone()
            };
            format!("http://{}:{}", host, self.config.server.port)
        };
        crate::dashboard::SettingsInfo {
            node_name: self.config.node.name.clone(),
            storage_path: crate::paths::data_dir().display().to_string(),
            max_size_gb: self.config.storage.max_size_gb,
            segment_duration: self.config.streaming.hls.segment_duration,
            fps: self.config.streaming.fps,
            encoder: self.config.streaming.encoder.clone(),
            hls_enabled: self.config.streaming.hls.enabled,
            heartbeat_interval: self.config.cloud.heartbeat_interval,
            motion_enabled: self.config.motion.enabled,
            motion_sensitivity: self.config.motion.threshold,
            motion_cooldown: self.config.motion.cooldown_secs,
            mode: self.config.mode,
            local_url,
        }
    }

    /// Run the node with the live TUI dashboard.
    ///
    /// This is the path the binary takes when launched from a terminal —
    /// the dashboard renders, raw-mode input is handled, and Ctrl+C
    /// triggers shutdown. The Windows Service path uses
    /// [`Self::run_headless`] instead because services have no terminal
    /// and SCM owns shutdown signalling.
    pub async fn run(self) -> Result<()> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.run_internal(stop_flag, /* render_tui = */ true).await
    }

    /// Run the node without the TUI render/input loop.
    ///
    /// Used by [`crate::service`] (Windows Service entry point). The
    /// caller owns the `stop_flag` and flips it when SCM signals Stop.
    /// Logging still flows through the Dashboard's tracing layer (so the
    /// SQLite log buffer keeps populating); the only difference from
    /// [`Self::run`] is that nothing tries to enable crossterm raw-mode
    /// or write to stdout, both of which would hang under SCM.
    pub async fn run_headless(self, stop_flag: Arc<AtomicBool>) -> Result<()> {
        self.run_internal(stop_flag, /* render_tui = */ false).await
    }

    async fn run_internal(
        mut self,
        stop_flag: Arc<AtomicBool>,
        render_tui: bool,
    ) -> Result<()> {
        // ── Create dashboard ────────────────────────────────────────────────
        let node_id = self.config.node.node_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let dash = Dashboard::new(&node_id, &self.config.cloud.api_url);
        dash.set_settings(self.build_settings_info());
        dash.set_db(self.db.clone(), self.hls_output_dir.clone());
        // Pass the API client so `/wipe confirm` can ask the backend
        // to delete our node record before we erase local state.
        // Without this the backend would keep the node row forever as
        // "offline" and happily re-pair on the next setup run.
        dash.set_api_client(self.api_client.clone());
        dash.load_logs_from_db();

        // Install dashboard into the tracing layer so all tracing::info!() etc.
        // calls throughout the codebase flow into the TUI + SQLite.
        crate::logging::set_dashboard(dash.clone());

        // ── 1. Detect cameras ────────────────────────────────────────────────
        dash.log_info("Detecting cameras…");
        let detected_cameras = self.detect_cameras(&dash).await?;

        // Register cameras in dashboard (camera_id filled in post-registration)
        for cam in &detected_cameras {
            dash.add_camera(CameraState {
                name: cam.name.clone(),
                camera_id: String::new(),
                resolution: format!("{}×{}", cam.preferred_resolution.0, cam.preferred_resolution.1),
                video_codec: String::new(),
                audio_codec: String::new(),
                status: CameraStatus::Starting,
                segments_uploaded: 0,
                bytes_uploaded: 0,
            });
        }

        // ── 2. Register with cloud (or fabricate locally) ───────────────────
        // Connected mode: round-trip to Command Center to register the
        // node + receive its assigned node_id and per-camera id mapping.
        // Local mode: skip the network call.  Use the stable
        // `local_node_id` SQLite KV row (created on first Local-mode
        // boot) as the namespace prefix; camera_mapping stays empty so
        // the per-camera fallback below generates IDs from
        // `<local_node_id>_<sanitized_device_path>`.
        let (node_id, camera_mapping) = if self.config.mode.is_local() {
            // Get-or-create the persistent local node id.  8 hex chars
            // matches the format Command Center registration returns,
            // so all the downstream string formatting (status bar
            // truncation to 8 chars, camera-id namespacing) keeps
            // working without special-casing.
            let local_id = match self.db.get_config("local_node_id")? {
                Some(id) => id,
                None => {
                    let id: String = uuid::Uuid::new_v4()
                        .simple()
                        .to_string()
                        .chars()
                        .take(8)
                        .collect();
                    self.db.set_config("local_node_id", &id)?;
                    dash.log_info(format!(
                        "Generated local node id: {}",
                        id.cyan().bold()
                    ));
                    id
                }
            };
            dash.log_info(format!(
                "Local mode — skipping Command Center registration (node {})",
                local_id.cyan().bold()
            ));
            (local_id, HashMap::<String, String>::new())
        } else {
            dash.log_info("Registering with cloud…");
            let registration = self.register_with_cloud(&detected_cameras, &dash).await?;
            let node_id = registration.node_id.clone();
            let camera_mapping: HashMap<String, String> = registration.cameras;
            // Surface the plan in the status bar. Advisory only — see the doc
            // comment on RegisterResponse::plan.  `None` on older backends that
            // don't send the field, in which case the dashboard hides the pill.
            dash.set_plan(registration.plan);
            dash.log_info(format!("Registered as node {}", node_id.cyan().bold()));

            // If the backend dropped any cameras for being over the plan cap,
            // surface it both as a pre-TUI yellow panel (visible in scrollback
            // after the dashboard takes over) and as a persistent WARN line in
            // the log buffer. Pure UX — enforcement already happened server-side.
            if let Some(hit) = registration.plan_limit_hit.as_ref() {
                dash.log_warn(format!(
                    "Plan limit reached: {} camera(s) skipped — {}",
                    hit.skipped.len(),
                    hit.detail,
                ));
                crate::setup::recovery::show_plan_limit_hit(hit, &self.config.cloud.api_url);
            }

            // Attach cloud-assigned camera_id to each dashboard row so per-ID
            // lookups (e.g. the heartbeat's real-status read) can find them.
            for cam in &detected_cameras {
                if let Some(cid) = camera_mapping.get(&cam.device_path) {
                    dash.set_camera_id(&cam.name, cid);
                }
            }

            (node_id, camera_mapping)
        };

        // ── 3. Start HLS streams ──────────────────────────────────────────────
        let mut running_streams: Vec<RunningStream> = Vec::new();
        let mut cameras_with_hls: Vec<(String, PathBuf)> = Vec::new();
        let recording_state: Arc<RwLock<HashSet<String>>> =
            Arc::new(RwLock::new(HashSet::new()));

        // Phase B: in Local mode, seed the recording set from the
        // `local_recording_state` SQLite table so per-camera record
        // toggles survive restarts.  In Connected mode this seed gets
        // overwritten ~30 s later by the heartbeat reconciler (which
        // is the canonical source of truth there), so seeding is a
        // no-op there but harmless.
        if self.config.mode.is_local() {
            match self.db.get_local_recording_state() {
                Ok(map) => {
                    if let Ok(mut set) = recording_state.write() {
                        for (cam, enabled) in map {
                            if enabled {
                                set.insert(cam);
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load local recording state from DB: {} — continuing with empty set",
                        e,
                    );
                }
            }
        }

        // Shared shutdown flag — set on Ctrl+C, /quit, or by the Windows
        // Service Control Manager via the public run_headless entry point.
        // Owned by the caller now (was created here in older versions);
        // every supervisor task gets a clone below.

        // Motion event channel — uploaders send, WebSocket client receives
        let (motion_tx, motion_rx) = tokio::sync::mpsc::channel::<
            crate::streaming::hls_uploader::MotionEvent,
        >(64);

        // Clean all HLS directories on startup so segment numbering resets fresh
        if let Ok(entries) = std::fs::read_dir(&self.hls_output_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }

        // Retired encoders — names we used to persist but have since
        // removed from the auto-detect pool.  If a prior install saved
        // one of these to the DB, clear it so auto-detect runs again.
        //
        // `h264_v4l2m2m` (Raspberry Pi V4L2 M2M) was retired in v0.1.14
        // because it writes a non-conforming SPS across every Pi hardware
        // revision we tested.  See HlsGenerator::detect_hw_encoder for the
        // full rationale.  Without this coercion, a Pi that completed
        // setup on v0.1.12 would keep using v4l2m2m forever — the runner
        // only re-detects when the DB value is empty.
        const RETIRED_ENCODERS: &[&str] = &["h264_v4l2m2m"];
        if RETIRED_ENCODERS
            .iter()
            .any(|e| *e == self.config.streaming.encoder)
        {
            let retired = self.config.streaming.encoder.clone();
            dash.log_warn(format!(
                "Retired encoder '{}' in config — clearing for re-detection",
                retired
            ));
            tracing::warn!(
                "Retired encoder '{}' found in DB; clearing for re-detection",
                retired
            );
            self.config.streaming.encoder.clear();
            let _ = self.db.delete_config("encoder");
        }

        // Detect encoder once (not per-camera) and persist it
        if self.config.streaming.encoder.is_empty() {
            let ffmpeg_path = crate::streaming::HlsGenerator::find_ffmpeg();
            if let Some(enc) = crate::streaming::HlsGenerator::detect_hw_encoder(&ffmpeg_path) {
                dash.log_info(format!("Hardware encoder detected: {}", enc.cyan()));
                self.config.streaming.encoder = enc.clone();
                // Save to DB so future runs skip auto-detect
                if let Err(e) = self.db.set_config("encoder", &enc) {
                    tracing::warn!("Failed to save encoder to DB: {}", e);
                }
            } else {
                dash.log_info("No hardware encoder found, using software (libx264)");
                self.config.streaming.encoder = "libx264".to_string();
                let _ = self.db.set_config("encoder", "libx264");
            }
        }

        // Update settings display with detected encoder
        dash.set_settings(self.build_settings_info());

        for detected in detected_cameras {
            let camera_id = camera_mapping.get(&detected.device_path)
                .cloned()
                .unwrap_or_else(|| {
                    let sanitized = detected.device_path
                        .replace("/", "_")
                        .replace("\\", "_")
                        .replace(" ", "_")
                        .trim_start_matches('_')
                        .to_string();
                    format!("{}_{}", node_id, sanitized)
                });

            let camera_hls_dir = self.hls_output_dir.join(&camera_id);
            std::fs::create_dir_all(&camera_hls_dir)?;

            if self.config.streaming.hls.enabled {
                let mut hls_config = HlsGeneratorConfig::from(self.config.streaming.hls.clone());
                hls_config.output_dir = camera_hls_dir.clone();
                hls_config.fps = self.config.streaming.fps;
                hls_config.encoder = self.config.streaming.encoder.clone();

                cameras_with_hls.push((camera_id.clone(), camera_hls_dir.clone()));

                // Start motion detection only on real-camera paths; we
                // can't know for certain at this point whether the
                // supervisor will end up falling back to a test pattern,
                // but motion on testsrc is meaningless anyway and the
                // uploader has its own cheap no-op when no frames arrive.
                let motion_cfg = self.config.motion.clone();

                // Build the uploader task (unchanged — it polls for
                // segments on disk regardless of who's writing them).
                // `is_local` flips the segment-push fast path: in Local
                // mode `SegmentUploader::push_segment` returns
                // Ok(true) without ever opening a network connection,
                // and the local archive write to encrypted SQLite (for
                // playback through the local web UI) runs as usual.
                let uploader_config = HlsUploaderConfig::new(camera_id.clone(), camera_hls_dir)
                    .with_local(self.config.mode.is_local());
                let cam_name = detected.name.clone();
                let uploader = HlsUploader::new(
                    uploader_config,
                    self.api_client.clone(),
                    recording_state.clone(),
                    self.db.clone(),
                    motion_cfg,
                    motion_tx.clone(),
                );
                // Shared between uploader and supervisor.  The uploader
                // sets it after ~20s of no new segments; the supervisor
                // watches it and kills FFmpeg so the restart path fires.
                // Without this, a wedged-but-alive FFmpeg (V4L2 deadlock,
                // thermal throttle, USB starvation) would leave the
                // camera dark in the UI forever since try_wait() only
                // trips on process exit.
                let stall_flag = Arc::new(AtomicBool::new(false));

                let dash_for_uploader = dash.clone();
                let camera_id_for_uploader = camera_id.clone();
                let stall_for_uploader = stall_flag.clone();
                let upload_handle = tokio::spawn(async move {
                    if let Err(e) = uploader.start_with_dashboard(
                        dash_for_uploader, cam_name, camera_id_for_uploader,
                        stall_for_uploader,
                    ).await {
                        tracing::error!("HLS uploader error: {}", e);
                    }
                });

                // Build and spawn the FFmpeg supervisor. It owns the
                // HlsGenerator, starts FFmpeg, polls for exits, and
                // restarts with backoff (or gives up → Failed). Replaces
                // the fire-and-forget inline start that used to leave
                // a crashed camera silently offline forever.
                let sup_cfg = SupervisorConfig {
                    hls_config,
                    primary: PipelineSource::Device(detected.device_path.clone()),
                    fallback: Some(PipelineSource::TestPattern(
                        self.config.streaming.hls.segment_duration * 100,
                        self.config.streaming.hls.segment_duration * 75,
                        self.config.streaming.fps,
                    )),
                    camera_name: detected.name.clone(),
                    camera_id: camera_id.clone(),
                    stall_flag: stall_flag.clone(),
                };
                let dash_for_sup = dash.clone();
                let stop_for_sup = stop_flag.clone();
                let supervisor_handle = tokio::spawn(async move {
                    supervise_hls(sup_cfg, dash_for_sup, stop_for_sup).await;
                });

                dash.log_info(format!("Started supervised HLS for {}", detected.name.cyan()));
                running_streams.push(RunningStream { supervisor_handle, upload_handle });
            }
        }

        // ── 4. Start HTTP server ──────────────────────────────────────────────
        // Build the Phase B `/api/*` state alongside the HLS map.  Both
        // modes (Local + Connected) get the same API surface — the
        // recording-toggle endpoint returns 409 in Connected mode (CC
        // is the source of truth) but everything else works identically.
        let camera_map: HashMap<String, PathBuf> = cameras_with_hls.into_iter().collect();
        let api_state = crate::server::LocalApiState::new(
            dash.clone(),
            self.db.clone(),
            recording_state.clone(),
            self.config.mode,
            self.hls_output_dir.clone(),
        );
        let http_server = crate::server::HttpServer::new_with_api(
            self.config.server.clone(),
            camera_map,
            api_state,
        );
        tokio::spawn(async move {
            if let Err(e) = http_server.run().await {
                tracing::error!("HTTP server error: {}", e);
            }
        });

        dash.log_info(format!(
            "Node online — streaming {} camera(s)",
            running_streams.len()
        ));

        // ── 5. Start heartbeat + WebSocket ───────────────────────────────────
        // Connected mode only — Local mode never registers, never
        // heartbeats, never opens the inbound-command WebSocket.
        // FFmpeg + HLS supervisor + retention loop + HTTP server all
        // keep running unchanged so the local web UI (Phase B+) sees
        // the same state.
        //
        // The motion_rx receiver still has to be consumed somewhere or
        // the bounded-mpsc channel fills up after ~64 events and the
        // uploader's `motion_tx.try_send` starts erroring (harmless,
        // just noisy in logs).  Phase B converts this channel to a
        // broadcast::Sender so multiple subscribers can listen; for
        // now in Local mode we drain the receiver in a tiny task.
        let streaming_camera_ids: Vec<String> = camera_mapping.values().cloned().collect();
        let (heartbeat_handle, ws_handle) = if self.config.mode.is_connected() {
            let hb = self.start_heartbeat_loop(
                dash.clone(),
                streaming_camera_ids.clone(),
                recording_state.clone(),
            );

            // Start WebSocket command channel (runs alongside HTTP heartbeats).
            // Recording state used to be plumbed through here for the
            // start_recording / stop_recording commands; both were retired
            // in v0.1.43 in favour of the heartbeat-driven reconciler in
            // start_heartbeat_loop, so the WS task no longer needs it.
            let ws = {
                let api_url = self.config.cloud.api_url.clone();
                let api_key = self.config.cloud.api_key.clone();
                let ws_node_id = node_id.clone();
                let ws_camera_ids = streaming_camera_ids;
                let ws_interval = self.config.cloud.heartbeat_interval;
                let ws_dash = dash.clone();
                let ws_hls_dir = self.hls_output_dir.clone();
                let ws_db = self.db.clone();
                tokio::spawn(async move {
                    crate::api::websocket::run_ws_client(
                        api_url,
                        api_key,
                        ws_node_id,
                        ws_camera_ids,
                        ws_interval,
                        ws_dash,
                        ws_hls_dir,
                        ws_db,
                        motion_rx,
                    ).await;
                })
            };
            (hb, ws)
        } else {
            // Local-mode placeholders.  Both tasks just await shutdown
            // so the existing `select!` / await-pattern in the rest of
            // run_internal doesn't need to grow a `Option<JoinHandle>`.
            let _ = streaming_camera_ids;
            let mut motion_rx_drain = motion_rx;
            let drain_handle = tokio::spawn(async move {
                while motion_rx_drain.recv().await.is_some() {
                    // Discard — Phase B turns this into a broadcast
                    // channel that feeds an SSE endpoint.
                }
            });
            // Make a no-op heartbeat handle so the function's later
            // joins don't need a separate code path.
            let noop_hb = tokio::spawn(async move {
                // Park forever; cancelled on shutdown by abort() in the
                // shutdown block below.
                std::future::pending::<()>().await;
            });
            (noop_hb, drain_handle)
        };

        // ── 6. Start retention cleanup (enforce max_size_gb) ──────────────────
        let retention_handle = {
            let ret_db = self.db.clone();
            let max_bytes = self.config.storage.max_size_gb * 1024 * 1024 * 1024;
            let ret_dash = dash.clone();
            tokio::spawn(async move {
                let interval = tokio::time::Duration::from_secs(5 * 60); // every 5 minutes
                loop {
                    tokio::time::sleep(interval).await;
                    match ret_db.enforce_retention(max_bytes) {
                        Ok((current, freed)) => {
                            if freed > 0 {
                                let freed_mb = freed / (1024 * 1024);
                                let current_gb = current / (1024 * 1024 * 1024);
                                ret_dash.log_info(format!(
                                    "Retention: freed {} MB, now {} GB", freed_mb, current_gb
                                ));
                            }
                        }
                        Err(e) => {
                            ret_dash.log_warn(format!("Retention check failed: {}", e));
                        }
                    }
                    // Prune old log entries (keep last 10,000)
                    let _ = ret_db.prune_logs(10_000);
                }
            })
        };

        // ── 7. Start dashboard render loop in a background thread ─────────────
        // Skip in headless mode (Windows Service): there's no terminal,
        // and `run_render_loop` calls `crossterm::terminal::enable_raw_mode`
        // which fails or hangs without a real TTY. The dashboard *object*
        // still exists and still receives tracing events — the SQLite log
        // buffer keeps populating — we just don't render.
        let render_thread = if render_tui {
            let stop_clone = stop_flag.clone();
            let dash_clone = dash.clone();
            Some(std::thread::spawn(move || {
                dash_clone.run_render_loop(Duration::from_millis(500), stop_clone);
            }))
        } else {
            None
        };

        // ── 8. Wait for shutdown (Ctrl+C from OS or /quit from dashboard) ────
        // In raw mode Ctrl+C is captured by the dashboard input loop and sets
        // the stop flag directly, so we poll it alongside the OS signal.
        // In headless mode, the caller (service main) flips stop_flag from
        // the SCM event handler — Ctrl+C is still installed as a fallback
        // for cases where the binary is run as `sourcebox-sentry-cloudnode service`
        // from a console for debugging.
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                dash.log_warn("Shutdown signal received — stopping…");
                stop_flag.store(true, Ordering::Relaxed);
            }
            _ = async {
                while !stop_flag.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            } => {}
        }

        // Give dashboard one last render cycle to show shutdown message.
        // No-op in headless mode (no thread to join).
        if let Some(handle) = render_thread {
            std::thread::sleep(Duration::from_millis(600));
            let _ = handle.join();
        }

        // Stop streams. The supervisor tasks already saw `stop_flag` flip
        // above and should be in the middle of killing FFmpeg cleanly
        // (HlsGenerator::stop inside the supervisor's loop). We still
        // abort() as a belt-and-suspenders backstop in case any task is
        // wedged in a sleep / syscall we can't interrupt.
        for stream in running_streams {
            stream.supervisor_handle.abort();
            stream.upload_handle.abort();
        }
        heartbeat_handle.abort();
        ws_handle.abort();
        retention_handle.abort();

        // Headless mode runs without a terminal, so writing to stdout is
        // pointless and would clutter the service log if stdout were
        // redirected to it. The shutdown is already reflected in the
        // SQLite log buffer + tracing-appender file.
        if render_tui {
            println!("\n  {}", "CloudNode stopped.".yellow());
        } else {
            tracing::info!("CloudNode stopped (headless).");
        }

        Ok(())
    }

    /// Run once (test mode)
    pub async fn run_once(mut self) -> Result<()> {
        let dash = Dashboard::new(
            self.config.node.node_id.as_deref().unwrap_or("test"),
            &self.config.cloud.api_url,
        );

        let detected_cameras = self.detect_cameras(&dash).await?;
        for cam in &detected_cameras {
            println!("  Camera: {} ({})", cam.name.cyan(), cam.device_path);
            println!("    Resolution: {}x{}", cam.preferred_resolution.0, cam.preferred_resolution.1);
        }

        match self.register_with_cloud(&detected_cameras, &dash).await {
            Ok(reg) => println!("  Registered: {}", reg.node_id.green()),
            Err(e)  => println!("  API not available: {}", e),
        }

        println!("\n  Testing heartbeat…");
        match self.test_heartbeat().await {
            Ok(r) => {
                println!("  Heartbeat: {}", r.timestamp.green());
                if r.key_rotated {
                    println!("  {} API key was rotated — update your config!", "⚠".yellow());
                }
            }
            Err(e) => println!("  Heartbeat failed: {}", e),
        }

        Ok(())
    }

    fn start_heartbeat_loop(
        &self,
        dash: Dashboard,
        camera_ids: Vec<String>,
        recording_state: Arc<RwLock<HashSet<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        let api_url = self.config.cloud.api_url.clone();
        let api_key = self.config.cloud.api_key.clone();
        let node_id = self.config.node.node_id.clone();
        let interval = self.config.cloud.heartbeat_interval;
        // Storage stats inputs.  The DB handle is cheap to clone (it's
        // an Arc internally), the cap doesn't change at runtime, and
        // the data_dir is the canonical resolver — same path the
        // retention loop uses.
        let stats_db = self.db.clone();
        let max_bytes = self.config.storage.max_size_gb * 1024 * 1024 * 1024;
        let data_dir = crate::paths::data_dir();

        tokio::spawn(async move {
            let mut client = match ApiClient::new(&api_url, &api_key) {
                Ok(c) => c,
                Err(e) => {
                    dash.log_error(format!("Heartbeat client error: {}", e));
                    return;
                }
            };
            if let Some(id) = &node_id {
                client.set_node_id(id.clone());
            }
            let local_ip = get_local_ip();

            // Track the last "update available" hint we logged so we don't
            // spam the dashboard with the same warning every 30s.  We log
            // when the value first appears AND when it changes (e.g. a
            // newer release lands while CloudNode is still running).
            let mut last_update_hint: Option<String> = None;

            loop {
                // Build a fresh snapshot each tick from the supervisor's
                // reported state. Unknown IDs fall back to "streaming"
                // so the backend doesn't see a suddenly-empty payload
                // during the brief window between registration and the
                // first supervisor status write.
                let camera_statuses: Vec<crate::api::CameraStatus> = camera_ids
                    .iter()
                    .map(|id| match dash.get_camera_status_by_id(id) {
                        Some(s) => {
                            let (wire, err) = s.to_wire();
                            crate::api::CameraStatus {
                                camera_id: id.clone(),
                                status: wire.to_string(),
                                last_error: err,
                            }
                        }
                        None => crate::api::CameraStatus {
                            camera_id: id.clone(),
                            status: "streaming".to_string(),
                            last_error: None,
                        },
                    })
                    .collect();

                // Collect storage stats fresh for this heartbeat.  total_size()
                // is a cheap SUM query on indexed columns, and the disk_info
                // lookup is O(disks) — both fast enough to do per-tick rather
                // than maintain a separate cache.  Side effect: updates the
                // global recording-pause flag based on the safety floor.
                let storage_stats = match stats_db.total_size() {
                    Ok(used) => Some(crate::storage::StorageStats::collect(
                        used, max_bytes, &data_dir,
                    )),
                    Err(e) => {
                        // Storage stats are nice-to-have; a DB read failure
                        // shouldn't block heartbeats from sending.  Log + carry
                        // on with no stats this tick.
                        tracing::debug!("storage stats collection skipped: {}", e);
                        None
                    }
                };

                match client.heartbeat_with_retry(
                    local_ip.as_deref(),
                    camera_statuses,
                    storage_stats,
                    3,
                ).await {
                    Ok(r) => {
                        if r.key_rotated {
                            if let Some(new_key) = r.new_api_key {
                                dash.log_warn("API key rotated by server — updating");
                                client.update_api_key(new_key);
                            }
                        }
                        // Propagate any plan update (e.g. the operator upgraded
                        // Free → Pro in Clerk) so the status-bar badge follows
                        // without requiring a re-register.  Skip when the
                        // backend omits the field — a rollback that drops the
                        // wire field shouldn't clobber the badge we set at
                        // register time.
                        if r.plan.is_some() {
                            dash.set_plan(r.plan.clone());
                        }
                        // Push the authoritative set of plan-suspended cameras
                        // into the dashboard.  Replaces the previous set
                        // wholesale on every heartbeat, so a newly-disabled
                        // camera shows up within one heartbeat and a newly-
                        // re-enabled one clears on the same cadence. The
                        // setter logs transitions — steady state is silent.
                        dash.set_disabled_cameras(r.disabled_cameras.clone());

                        // Reconcile the in-memory recording_state with the
                        // backend's authoritative answer (v0.1.43+).  The
                        // backend computes "should this camera be recording
                        // right now?" from the operator-set policy on each
                        // Camera row — continuous_24_7 OR (scheduled AND
                        // in-window).  We replace the local set wholesale,
                        // so a manual record-button press, a Continuous-24/7
                        // toggle, or the start of a scheduled window all
                        // converge within one tick.
                        //
                        // Self-healing: a node that crashes loses its in-
                        // memory set, but the next heartbeat re-asserts the
                        // correct state.  No imperative WebSocket commands
                        // involved.
                        //
                        // Older backends omit `recording_state` entirely —
                        // we treat that as "no info, leave the current
                        // set alone" so a backend rollback can't silently
                        // disable archive on every connected node.
                        if let Some(target) = r.recording_state.as_ref() {
                            if let Ok(mut set) = recording_state.write() {
                                set.clear();
                                for (cam_id, should_record) in target {
                                    if *should_record {
                                        set.insert(cam_id.clone());
                                    }
                                }
                            }
                        }
                        // Surface "update available" hints from the backend
                        // exactly once per distinct version so the operator
                        // sees the nudge without the heartbeat loop turning
                        // it into a once-per-30s wall of warnings.
                        if r.update_available != last_update_hint {
                            if let Some(latest) = &r.update_available {
                                dash.log_warn(format!(
                                    "CloudNode update available: {} (running {}). Re-run the installer to update.",
                                    latest,
                                    env!("CARGO_PKG_VERSION"),
                                ));
                            }
                            last_update_hint = r.update_available.clone();
                        }
                        // Heartbeat success is silent — no log noise every N seconds
                    }
                    Err(e) => dash.log_error(format!("Heartbeat failed: {}", e)),
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        })
    }

    async fn test_heartbeat(&self) -> Result<crate::api::HeartbeatResponse> {
        let mut client = ApiClient::new(&self.config.cloud.api_url, &self.config.cloud.api_key)?;
        if let Some(id) = &self.config.node.node_id {
            client.set_node_id(id.clone());
        }
        // test_heartbeat is the boot-time smoke test before the main loop
        // takes over. No storage stats yet — they get reported on the first
        // real heartbeat tick.
        client.heartbeat_with_retry(get_local_ip().as_deref(), vec![], None, 3).await
    }

    async fn detect_cameras(&self, dash: &Dashboard) -> Result<Vec<DetectedCamera>> {
        let cameras = camera::detect_cameras()?;
        if cameras.is_empty() {
            dash.log_warn("No cameras detected");
        } else {
            dash.log_info(format!("Detected {} camera(s)", cameras.len()));
        }
        Ok(cameras)
    }

    async fn register_with_cloud(
        &mut self,
        cameras: &[DetectedCamera],
        dash: &Dashboard,
    ) -> Result<crate::api::RegisterResponse> {
        let camera_infos: Vec<crate::api::CameraInfo> = cameras.iter().map(|c| c.clone().into()).collect();

        let node_id = match self.config.node.node_id.as_ref() {
            Some(id) => id,
            None => {
                let error = crate::setup::recovery::RegistrationError::ConfigError {
                    message: "Node ID not configured. Run setup first.".to_string(),
                };
                crate::setup::recovery::show_registration_error(&error);
                if maybe_offer_reset(&error, &self.db) {
                    return Err(crate::error::Error::ResetRequested);
                }
                return Err(crate::error::Error::AlreadyReported);
            }
        };

        let codec_info = if let Some(cam) = cameras.first() {
            match crate::streaming::codec_detector::detect_codec_from_camera(&cam.device_path) {
                Ok(info) => {
                    dash.log_info(format!(
                        "Codec detected: {} / {}",
                        info.video_codec.cyan(),
                        info.audio_codec.cyan()
                    ));
                    Some(info)
                }
                Err(e) => {
                    dash.log_warn(format!("Codec detection failed: {}", e));
                    None
                }
            }
        } else {
            None
        };

        match self.api_client.register(
            node_id,
            &self.config.node.name,
            camera_infos,
            codec_info.as_ref().map(|c| c.video_codec.as_str()),
            codec_info.as_ref().map(|c| c.audio_codec.as_str()),
        ).await {
            Ok(r) => Ok(r),
            Err(e) => {
                let msg = e.to_string();
                let api_url = self.config.cloud.api_url.clone();
                let reg_err = if msg.contains("404") || msg.contains("not found") {
                    crate::setup::recovery::RegistrationError::InvalidNodeId {
                        node_id: node_id.clone(),
                        api_url,
                    }
                } else if msg.contains("401") || msg.contains("403") {
                    crate::setup::recovery::RegistrationError::InvalidApiKey {
                        node_id: node_id.clone(),
                        api_url,
                    }
                } else if msg.contains("connect") || msg.contains("timeout") {
                    crate::setup::recovery::RegistrationError::NetworkError {
                        api_url,
                        message: msg.clone(),
                    }
                } else {
                    crate::setup::recovery::RegistrationError::ServerError {
                        code: 0,
                        message: msg.clone(),
                    }
                };
                crate::setup::recovery::show_registration_error(&reg_err);
                if maybe_offer_reset(&reg_err, &self.db) {
                    Err(crate::error::Error::ResetRequested)
                } else {
                    Err(crate::error::Error::AlreadyReported)
                }
            }
        }
    }

}

fn get_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

/// For registration errors where a fresh reset would help (bad credentials,
/// broken config), ask the user interactively whether to wipe the stored
/// credentials. The wipe runs against the live SQLite handle rather than
/// deleting the file (Windows holds an exclusive-share lock on the open db).
///
/// Returns `true` if the user confirmed and credentials were wiped — the
/// caller should propagate `Error::ResetRequested` so `main` can tear this
/// node down, re-run the setup wizard, and retry with fresh credentials.
/// Returns `false` if the prompt wasn't offered (non-credential error or
/// non-interactive session) or if the user declined.
fn maybe_offer_reset(
    error: &crate::setup::recovery::RegistrationError,
    db: &NodeDatabase,
) -> bool {
    if !error.offers_reset() {
        return false;
    }
    crate::setup::recovery::prompt_for_reset(db)
        == crate::setup::recovery::ResetOutcome::Reset
}
