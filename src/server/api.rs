// Sentinel CloudNode - Camera streaming node for Sentinel Command Center
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
//! Phase B local web-UI HTTP API.
//!
//! All routes mounted under `/api/*` on the existing warp server in
//! [`super::http`].  Powers the Phase C SPA (live grid, snapshot
//! capture, per-camera recording toggle, recording playback, status).
//!
//! ## Threat model — no auth in v1
//!
//! - `bind = 127.0.0.1` (Connected default): only same-host processes
//!   can hit `/api/*`.  Anyone with shell access on the box could
//!   already wipe `data/node.db` directly, so the additional surface
//!   is not meaningfully larger.
//! - `bind = 0.0.0.0` (Local default — set by the setup wizard):
//!   anyone on the LAN can read live HLS, snapshots, recordings, and
//!   toggle the local recording flag.  Acceptable for v1's
//!   home/small-business LAN target.  **Add auth before exposing
//!   this server to the public internet.**

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use warp::filters::BoxedFilter;
use warp::{Filter, Rejection};

use crate::config::NodeMode;
use crate::dashboard::{CameraStatus, Dashboard};
use crate::storage::NodeDatabase;

/// Uniform reply type used by every `/api/*` handler.  Concrete rather
/// than `Box<dyn Reply>` so warp's filter combinators can stitch the
/// chain together without lifetime gymnastics.
type ApiReply = warp::http::Response<Vec<u8>>;

/// Shared state plumbed into every `/api/*` handler.  Built once at boot
/// in [`crate::node::runner::Node::run_internal`] and cloned across
/// route filters via warp's `with_state` pattern.
#[derive(Clone)]
pub struct LocalApiState {
    pub dashboard: Dashboard,
    pub db: NodeDatabase,
    /// Shared with the HLS uploader.  In Local mode the
    /// `POST /api/cameras/{id}/recording` route mutates this set;
    /// the uploader reads it on every segment to decide whether to
    /// archive to SQLite.  In Connected mode the heartbeat reconciler
    /// owns the same set, so the recording route returns 409.
    pub recording_state: Arc<RwLock<std::collections::HashSet<String>>>,
    pub mode: NodeMode,
    pub hls_base_dir: PathBuf,
    pub uptime_start: std::time::Instant,
    pub node_version: &'static str,
    /// Command Center URL surfaced via `/api/status` so the SPA's
    /// Local-mode upsell footer + Connected-mode "Live view in CC"
    /// CTA can link to the right deployment without a hardcoded
    /// constant.  In Local mode this is the canonical default
    /// (operator hasn't paired yet).  In Connected mode it's the
    /// `config.cloud.api_url` the operator entered at setup.
    pub command_center_url: String,
}

/// Canonical Command Center URL used as the Local-mode default for
/// `LocalApiState.command_center_url`.  Operators in Connected mode
/// override this with whatever `config.cloud.api_url` was set to at
/// setup time.
pub const DEFAULT_COMMAND_CENTER_URL: &str = "https://opensentry-command.fly.dev";

impl LocalApiState {
    pub fn new(
        dashboard: Dashboard,
        db: NodeDatabase,
        recording_state: Arc<RwLock<std::collections::HashSet<String>>>,
        mode: NodeMode,
        hls_base_dir: PathBuf,
        cloud_api_url: String,
    ) -> Self {
        // Empty `cloud_api_url` happens in Local-mode installs that
        // never paired.  Fall back to the canonical default so the
        // SPA's upsell footer always has a link to send the operator
        // through.
        let command_center_url = if cloud_api_url.trim().is_empty() {
            DEFAULT_COMMAND_CENTER_URL.to_string()
        } else {
            cloud_api_url
        };
        Self {
            dashboard,
            db,
            recording_state,
            mode,
            hls_base_dir,
            uptime_start: std::time::Instant::now(),
            node_version: env!("CARGO_PKG_VERSION"),
            command_center_url,
        }
    }

    /// Returns true if `camera_id` is currently registered with the
    /// dashboard.  Used by the snapshot route to reject unknown ids
    /// before they reach the filesystem layer — without this check, a
    /// LAN attacker could pass an arbitrary path (e.g. `..%2F..%2Fetc`
    /// percent-decoded by warp's String extractor) into
    /// `hls_base_dir.join(camera_id)` and trick FFmpeg into reading
    /// files outside the HLS root.  The dashboard's camera list is
    /// populated synchronously in `runner::run_internal` before the
    /// HTTP server starts accepting requests, so this is race-free.
    pub fn is_known_camera_id(&self, camera_id: &str) -> bool {
        // Reject empty / suspiciously-shaped ids cheaply before taking
        // the dashboard lock.  The deterministic id formula is
        // `<8-hex>_<sanitised-device-path>` — letters, digits,
        // underscore, hyphen, dot.  Anything else (slashes, encoded
        // bytes, traversal sequences) shouldn't reach this code path
        // because the warp `String` extractor matches a single segment
        // — but layered defence is cheap.
        if camera_id.is_empty() || camera_id.len() > 256 {
            return false;
        }
        if !camera_id.bytes().all(|b| {
            b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
        }) {
            return false;
        }
        match self.dashboard.0.lock() {
            Ok(guard) => guard.cameras.iter().any(|c| c.camera_id == camera_id),
            Err(p) => p.into_inner().cameras.iter().any(|c| c.camera_id == camera_id),
        }
    }
}

/// Combine all `/api/*` route filters into a single boxed filter that
/// the HTTP server can chain after `/health` and `/hls/*`.  Every
/// handler returns the same `ApiReply` (a concrete
/// `warp::http::Response<Vec<u8>>`) so warp's `or().unify()` chain
/// works without runtime erasure.
pub fn routes(state: LocalApiState) -> BoxedFilter<(ApiReply,)> {
    list_cameras(state.clone())
        .or(post_snapshot(state.clone()))
        .unify()
        .or(list_snapshots(state.clone()))
        .unify()
        .or(get_snapshot(state.clone()))
        .unify()
        .or(delete_snapshot(state.clone()))
        .unify()
        .or(toggle_recording(state.clone()))
        .unify()
        .or(list_recordings(state.clone()))
        .unify()
        .or(recording_playlist(state.clone()))
        .unify()
        .or(recording_segment(state.clone()))
        .unify()
        .or(status(state))
        .unify()
        .boxed()
}

// ── Static SPA assets (Phase C) ────────────────────────────────────

/// Embedded `web-dist/` bundle.  Vite writes a single `index.html`
/// + `assets/<hash>.{js,css}` here; rust-embed picks them up at
/// compile time so the Rust binary ships the SPA as a single file.
/// The `debug-embed` feature flag (set in Cargo.toml) makes this work
/// for `cargo run` too.
#[derive(rust_embed::Embed)]
#[folder = "web-dist"]
struct WebAssets;

/// Build the static-asset filter chain.  Three branches:
///   - GET /           → embedded index.html
///   - GET /assets/*  → hashed JS/CSS/etc with their content-type
///                       inferred via `mime_guess`
///   - GET /*path     → SPA fallback (also serves index.html so
///                       `react-router` deep links resolve cleanly)
///
/// Mounted AFTER `/health`, `/hls/*`, and `/api/*` so those win on
/// path collisions.  Returns the same `ApiReply` type so the warp
/// `or` chain stays uniform.
pub fn static_routes() -> BoxedFilter<(ApiReply,)> {
    let root = warp::path::end().and(warp::get()).map(serve_index);

    let assets = warp::path("assets")
        .and(warp::path::tail())
        .and(warp::get())
        .map(|tail: warp::path::Tail| serve_asset(&format!("assets/{}", tail.as_str())));

    let spa_fallback = warp::path::tail()
        .and(warp::get())
        .map(|tail: warp::path::Tail| {
            let path = tail.as_str();
            // Anything that already starts with /api or /hls is
            // routed above and never reaches us.  But to keep this
            // filter robust when reordering, defensively reject those
            // prefixes here too — better than serving index.html for
            // a missing API path and confusing the SPA.
            if path.starts_with("api") || path.starts_with("hls") || path == "health" {
                return error_response(404, "not_found", "");
            }
            serve_index()
        });

    root.or(assets).unify().or(spa_fallback).unify().boxed()
}

fn serve_index() -> ApiReply {
    match WebAssets::get("index.html") {
        Some(file) => warp::http::Response::builder()
            .status(200)
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Cache-Control", "no-cache")
            .body(file.data.into_owned())
            .unwrap_or_else(|_| empty_response(500)),
        None => warp::http::Response::builder()
            .status(503)
            .header("Content-Type", "text/plain")
            .body(
                b"Web UI not built. Run `npm install && npm run build` in `web/` and rebuild the binary."
                    .to_vec(),
            )
            .unwrap_or_else(|_| empty_response(500)),
    }
}

fn serve_asset(path: &str) -> ApiReply {
    let Some(file) = WebAssets::get(path) else {
        return empty_response(404);
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    warp::http::Response::builder()
        .status(200)
        .header("Content-Type", mime.as_ref())
        // Vite hashes filenames so the bundle is content-addressed;
        // an aggressive cache header is safe and turns repeat loads
        // into 304s without a round-trip.
        .header("Cache-Control", "public, max-age=31536000, immutable")
        .body(file.data.into_owned())
        .unwrap_or_else(|_| empty_response(500))
}

// ── Helpers ─────────────────────────────────────────────────────────

fn with_state(
    state: LocalApiState,
) -> impl Filter<Extract = (LocalApiState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

fn json_response<T: Serialize>(value: &T, status: u16) -> ApiReply {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    warp::http::Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-cache")
        .body(body)
        .unwrap_or_else(|_| empty_response(500))
}

fn error_response(status: u16, error: &str, message: &str) -> ApiReply {
    json_response(
        &serde_json::json!({ "error": error, "message": message }),
        status,
    )
}

fn empty_response(status: u16) -> ApiReply {
    warp::http::Response::builder()
        .status(status)
        .body(Vec::new())
        .expect("empty response builds")
}

fn bytes_response(
    status: u16,
    content_type: &str,
    cache_control: &str,
    body: Vec<u8>,
) -> ApiReply {
    warp::http::Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Cache-Control", cache_control)
        .body(body)
        .unwrap_or_else(|_| empty_response(500))
}

// ── Route: GET /api/cameras ────────────────────────────────────────

#[derive(Serialize)]
struct CameraDto {
    id: String,
    name: String,
    resolution: String,
    status: String,
    last_error: Option<String>,
    video_codec: String,
    audio_codec: String,
    segments_uploaded: u64,
    bytes_uploaded: u64,
    hls_url: String,
    suspended: bool,
    recording: bool,
}

fn list_cameras(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "cameras")
        .and(warp::get())
        .and(with_state(state))
        .map(|st: LocalApiState| -> ApiReply {
            let dash_state = st
                .dashboard
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let recording = match st.recording_state.read() {
                Ok(r) => r.clone(),
                Err(_) => Default::default(),
            };
            let cameras: Vec<CameraDto> = dash_state
                .cameras
                .iter()
                .map(|c| {
                    let (status_label, last_error) = c.status.to_wire();
                    CameraDto {
                        id: c.camera_id.clone(),
                        name: c.name.clone(),
                        resolution: c.resolution.clone(),
                        status: String::from(status_label),
                        last_error,
                        video_codec: c.video_codec.clone(),
                        audio_codec: c.audio_codec.clone(),
                        segments_uploaded: c.segments_uploaded,
                        bytes_uploaded: c.bytes_uploaded,
                        hls_url: format!("/hls/{}/stream.m3u8", c.camera_id),
                        suspended: dash_state.disabled_cameras.contains(&c.camera_id),
                        recording: recording.contains(&c.camera_id),
                    }
                })
                .collect();
            json_response(&cameras, 200)
        })
}

// ── Route: POST /api/cameras/{id}/snapshot ─────────────────────────

fn post_snapshot(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "cameras" / String / "snapshot")
        .and(warp::post())
        .and(warp::header::optional::<String>("content-type"))
        .and(with_state(state))
        .and_then(|camera_id: String, content_type: Option<String>, st: LocalApiState| async move {
            // CSRF guard: with no required header this was the one
            // mutating route reachable as a CORS *simple request* — any
            // web page the operator visits could blind-POST it at
            // 127.0.0.1 (opaque response, but a real server-side FFmpeg
            // spawn + DB write per hit).  Requiring application/json
            // forces a cross-origin preflight, which this server never
            // approves; the SPA sends the header (web/src/lib/api.ts).
            let is_json = content_type
                .as_deref()
                .map(|ct| ct.split(';').next().unwrap_or("").trim() == "application/json")
                .unwrap_or(false);
            if !is_json {
                return Ok::<_, Rejection>(error_response(
                    415,
                    "content_type_required",
                    "POST with Content-Type: application/json (cross-origin CSRF guard)",
                ));
            }
            // Reject unknown ids before they hit the filesystem layer.
            // Without this check a path-traversal payload in `camera_id`
            // (warp's String extractor decodes `%2F`, `%2E%2E`, etc.)
            // would let `hls_base_dir.join(camera_id)` escape the HLS
            // root and have FFmpeg read arbitrary files.
            if !st.is_known_camera_id(&camera_id) {
                return Ok::<_, Rejection>(error_response(
                    404,
                    "unknown_camera",
                    "no camera with that id is currently registered",
                ));
            }
            let result = crate::api::commands::take_snapshot(
                &camera_id,
                &st.hls_base_dir,
                &st.db,
            )
            .await;
            let reply: ApiReply = match result {
                Ok(meta) if meta.id == 0 => {
                    // The capture succeeded but the DB write was skipped
                    // because the host disk is under the safety floor.
                    // Tell the operator clearly — without this, the SPA
                    // shows "captured" then 404s when the gallery tries
                    // to load /api/snapshots/0.
                    error_response(
                        503,
                        "disk_safety_floor",
                        "Snapshot captured but archive skipped — host disk is critically low. \
                         Free space in the data directory and try again.",
                    )
                }
                Ok(meta) => {
                    let body = serde_json::json!({
                        "id": meta.id,
                        "camera_id": meta.camera_id,
                        "filename": meta.filename,
                        "timestamp": meta.timestamp,
                        "size_bytes": meta.size_bytes,
                        "image_url": format!("/api/snapshots/{}", meta.id),
                    });
                    json_response(&body, 200)
                }
                Err(e) => error_response(503, "snapshot_failed", &e),
            };
            Ok::<_, Rejection>(reply)
        })
}

// ── Route: GET /api/snapshots ──────────────────────────────────────

fn list_snapshots(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "snapshots")
        .and(warp::get())
        .and(warp::query::<HashMap<String, String>>())
        .and(with_state(state))
        .map(|q: HashMap<String, String>, st: LocalApiState| -> ApiReply {
            let camera_id = q.get("camera_id").map(|s| s.as_str());
            match st.db.list_snapshots(camera_id) {
                Ok(snaps) => json_response(&snaps, 200),
                Err(e) => error_response(500, "db_error", &e.to_string()),
            }
        })
}

// ── Route: GET /api/snapshots/{id} ─────────────────────────────────

fn get_snapshot(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / i64)
        .and(warp::get())
        .and(with_state(state))
        .map(|id: i64, st: LocalApiState| -> ApiReply {
            match st.db.get_snapshot_data(id) {
                Ok(bytes) => bytes_response(200, "image/jpeg", "private, max-age=86400", bytes),
                Err(_) => error_response(404, "not_found", "snapshot not found"),
            }
        })
}

// ── Route: DELETE /api/snapshots/{id} ──────────────────────────────

fn delete_snapshot(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "snapshots" / i64)
        .and(warp::delete())
        .and(with_state(state))
        .map(|id: i64, st: LocalApiState| -> ApiReply {
            match st.db.delete_snapshot(id) {
                Ok(0) => error_response(404, "not_found", "snapshot not found"),
                Ok(_) => json_response(&serde_json::json!({ "deleted": id }), 200),
                Err(e) => error_response(500, "db_error", &e.to_string()),
            }
        })
}

// ── Route: POST /api/cameras/{id}/recording ────────────────────────

#[derive(serde::Deserialize)]
struct RecordingToggleBody {
    recording: bool,
}

fn toggle_recording(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "cameras" / String / "recording")
        .and(warp::post())
        .and(warp::body::json::<RecordingToggleBody>())
        .and(with_state(state))
        .map(
            |camera_id: String, body: RecordingToggleBody, st: LocalApiState| -> ApiReply {
                // Connected mode: CC heartbeat reconciler is the source
                // of truth — flipping the local set would be overwritten
                // ~30s later anyway, so reject loudly with 409.
                if st.mode.is_connected() {
                    return error_response(
                        409,
                        "recording_managed_by_command_center",
                        "Recording state is managed by Command Center in Connected mode. \
                         Change the camera's recording policy in the Command Center UI \
                         (Settings → Cameras) and the heartbeat reconciler will sync \
                         within ~30 seconds.",
                    );
                }
                // Reject unknown camera ids before mutating anything —
                // mirrors the snapshot route.  Without this a Local-mode
                // node (bound 0.0.0.0) accepts a toggle for ANY id: it
                // returns a misleading 200 for a camera that doesn't
                // exist, and — because set_local_recording upserts one row
                // per distinct camera_id and that table has no retention —
                // lets a LAN client grow local_recording_state unboundedly
                // with junk ids (which are then re-seeded into the
                // in-memory set on every boot).
                if !st.is_known_camera_id(&camera_id) {
                    return error_response(
                        404,
                        "unknown_camera",
                        "no camera with that id is currently registered",
                    );
                }
                // Local mode: flip in-memory set + persist for restart.
                if let Ok(mut set) = st.recording_state.write() {
                    if body.recording {
                        set.insert(camera_id.clone());
                    } else {
                        set.remove(&camera_id);
                    }
                }
                if let Err(e) = st.db.set_local_recording(&camera_id, body.recording) {
                    return error_response(500, "db_error", &e.to_string());
                }
                json_response(
                    &serde_json::json!({
                        "camera_id": camera_id,
                        "recording": body.recording,
                    }),
                    200,
                )
            },
        )
}

// ── Route: GET /api/recordings ─────────────────────────────────────

fn list_recordings(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "recordings")
        .and(warp::get())
        .and(warp::query::<HashMap<String, String>>())
        .and(with_state(state))
        .map(|q: HashMap<String, String>, st: LocalApiState| -> ApiReply {
            let camera_id = q.get("camera_id").map(|s| s.as_str());
            match st.db.list_recordings(camera_id) {
                Ok(recs) => json_response(&recs, 200),
                Err(e) => error_response(500, "db_error", &e.to_string()),
            }
        })
}

// ── Route: GET /api/recordings/{cam}/{date}/playlist.m3u8 ──────────

fn recording_playlist(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "recordings" / String / String / "playlist.m3u8")
        .and(warp::get())
        .and(with_state(state))
        .map(|cam: String, date: String, st: LocalApiState| -> ApiReply {
            // Defensive shape: date must be YYYY-MM-DD, no traversal.
            if !is_valid_date(&date) {
                return error_response(400, "bad_date", "date must be YYYY-MM-DD");
            }
            match st.db.list_recording_segment_seqs(&cam, &date) {
                Ok(rows) if rows.is_empty() => {
                    error_response(404, "not_found", "no segments for camera+date")
                }
                Ok(rows) => {
                    let body = build_m3u8(&rows);
                    bytes_response(
                        200,
                        "application/vnd.apple.mpegurl",
                        "no-cache",
                        body.into_bytes(),
                    )
                }
                Err(e) => error_response(500, "db_error", &e.to_string()),
            }
        })
}

// ── Route: GET /api/recordings/{cam}/{date}/segment_{n}.ts ─────────

fn recording_segment(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "recordings" / String / String / String)
        .and(warp::get())
        .and(with_state(state))
        .map(
            |cam: String, date: String, filename: String, st: LocalApiState| -> ApiReply {
                if !is_valid_date(&date) {
                    return error_response(400, "bad_date", "date must be YYYY-MM-DD");
                }
                let Some(seq) = parse_segment_filename(&filename) else {
                    return error_response(
                        400,
                        "bad_filename",
                        "filename must be segment_<digits>.ts",
                    );
                };
                match st.db.get_recording_segment(&cam, &date, seq) {
                    Ok(bytes) => bytes_response(
                        200,
                        "video/mp2t",
                        "private, max-age=86400",
                        bytes,
                    ),
                    Err(_) => error_response(404, "not_found", "segment not found"),
                }
            },
        )
}

// ── Route: GET /api/status ─────────────────────────────────────────

fn status(
    state: LocalApiState,
) -> impl Filter<Extract = (ApiReply,), Error = Rejection> + Clone {
    warp::path!("api" / "status")
        .and(warp::get())
        .and(with_state(state))
        .map(|st: LocalApiState| -> ApiReply {
            let dash = st
                .dashboard
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let total_bytes: u64 = dash.cameras.iter().map(|c| c.bytes_uploaded).sum();
            let camera_count = dash.cameras.len();
            // Treat anything that's NOT in a known-down state as active.
            // Mirrors getStats() on the Command Center dashboard.
            let active = dash
                .cameras
                .iter()
                .filter(|c| {
                    !dash.disabled_cameras.contains(&c.camera_id)
                        && !matches!(
                            c.status,
                            CameraStatus::Offline
                                | CameraStatus::Failed { .. }
                                | CameraStatus::Error(_)
                        )
                })
                .count();
            let plan = dash.plan.clone();
            let body = serde_json::json!({
                "mode": st.mode.as_str(),
                "version": st.node_version,
                "uptime_secs": st.uptime_start.elapsed().as_secs(),
                "node_id": dash.node_id.clone(),
                "camera_count": camera_count,
                "active_camera_count": active,
                "total_segments": dash.total_segments,
                "total_bytes_uploaded": total_bytes,
                "plan": plan,
                "command_center_url": st.command_center_url.clone(),
            });
            json_response(&body, 200)
        })
}

// ── M3U8 builder ───────────────────────────────────────────────────

/// Build a VOD HLS playlist from `(seq, duration_ms)` rows.  The
/// EXT-X-PLAYLIST-TYPE:VOD tag tells players this is a sealed
/// playlist — no live-edge polling, accurate seek bar.
fn build_m3u8(rows: &[(u64, u32)]) -> String {
    let max_dur_secs = rows
        .iter()
        .map(|(_, d)| (*d as f64 / 1000.0).ceil() as u32)
        .max()
        .unwrap_or(1)
        .max(1);
    let first_seq = rows.first().map(|(s, _)| *s).unwrap_or(0);
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:3\n");
    out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max_dur_secs));
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", first_seq));
    for (seq, dur_ms) in rows {
        let dur = (*dur_ms as f64) / 1000.0;
        out.push_str(&format!("#EXTINF:{:.3},\n", dur));
        out.push_str(&format!("segment_{:05}.ts\n", seq));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Strict YYYY-MM-DD shape.  Rejects traversal, encoded slashes, and
/// anything that would let us read across date boundaries.
fn is_valid_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Parse `segment_<digits>.ts` → seq.  Returns None on any other shape.
fn parse_segment_filename(filename: &str) -> Option<u64> {
    let middle = filename
        .strip_prefix("segment_")
        .and_then(|s| s.strip_suffix(".ts"))?;
    if middle.is_empty() || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    middle.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_m3u8_emits_vod_playlist() {
        let rows = vec![(1u64, 1000u32), (2, 1000), (3, 1000)];
        let out = build_m3u8(&rows);
        assert!(out.starts_with("#EXTM3U\n"));
        assert!(out.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(out.contains("#EXTINF:1.000,"));
        assert!(out.contains("segment_00001.ts"));
        assert!(out.contains("segment_00003.ts"));
        assert!(out.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn build_m3u8_handles_gappy_sequence() {
        // Real recordings can have gaps (FFmpeg restarts, retention).
        let rows = vec![(1u64, 1000u32), (2, 1000), (7, 1000), (8, 1000)];
        let out = build_m3u8(&rows);
        // Media sequence is the first seq actually present.
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:1"));
        assert!(out.contains("segment_00007.ts"));
        // Don't synthesise missing segments.
        assert!(!out.contains("segment_00003.ts"));
    }

    #[test]
    fn build_m3u8_target_duration_is_ceil_of_max() {
        // 1.7s → ceil=2.
        let rows = vec![(1u64, 1000u32), (2, 1700), (3, 900)];
        let out = build_m3u8(&rows);
        assert!(out.contains("#EXT-X-TARGETDURATION:2"));
        assert!(out.contains("#EXTINF:1.700,"));
    }

    #[test]
    fn is_valid_date_accepts_yyyy_mm_dd() {
        assert!(is_valid_date("2026-05-09"));
        assert!(is_valid_date("0000-00-00"));
    }

    #[test]
    fn is_valid_date_rejects_traversal_and_junk() {
        assert!(!is_valid_date(""));
        assert!(!is_valid_date("../../../etc/passwd"));
        assert!(!is_valid_date("2026/05/09"));
        assert!(!is_valid_date("2026-5-9"));
        assert!(!is_valid_date("2026-05-09  "));
        assert!(!is_valid_date("2026-05-09T"));
    }

    #[test]
    fn parse_segment_filename_accepts_well_formed() {
        assert_eq!(parse_segment_filename("segment_00001.ts"), Some(1));
        assert_eq!(parse_segment_filename("segment_99.ts"), Some(99));
    }

    #[test]
    fn parse_segment_filename_rejects_junk() {
        assert!(parse_segment_filename("../etc/passwd").is_none());
        assert!(parse_segment_filename("segment_.ts").is_none());
        assert!(parse_segment_filename("segment_abc.ts").is_none());
        assert!(parse_segment_filename("segment_1.mp4").is_none());
        assert!(parse_segment_filename("stream.m3u8").is_none());
    }

    #[test]
    fn web_assets_includes_index_html() {
        // Catches the failure mode where someone runs `cargo build`
        // without first running `npm run build` in `web/`. Without
        // this guard the binary would ship with the "Web UI not
        // built" placeholder and the SPA would 503 in production.
        let index = WebAssets::get("index.html");
        assert!(
            index.is_some(),
            "web-dist/index.html missing — run `npm install && npm run build` in `web/` before `cargo build`",
        );
        let body = index.unwrap().data;
        let html = std::str::from_utf8(&body).expect("index.html is utf8");
        assert!(html.contains("<div id=\"root\">"), "expected #root mount point");
    }
}

#[cfg(test)]
mod recording_toggle_tests {
    use super::*;
    use crate::dashboard::CameraState;
    use std::collections::HashSet;
    use std::sync::RwLock;

    /// Build a Local-mode LocalApiState with exactly one registered
    /// camera.  Returns the TempDir too so the on-disk SQLite file
    /// outlives the test body.
    fn local_state_with_camera(cam_id: &str) -> (LocalApiState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");
        let dash = Dashboard::new("node_test", "");
        {
            let mut s = dash.0.lock().expect("dash lock");
            s.add_camera(CameraState {
                name: "Front Door".to_string(),
                camera_id: cam_id.to_string(),
                resolution: "1280x720".to_string(),
                video_codec: "h264".to_string(),
                audio_codec: "none".to_string(),
                status: CameraStatus::Streaming,
                segments_uploaded: 0,
                bytes_uploaded: 0,
            });
        }
        let state = LocalApiState::new(
            dash,
            db,
            Arc::new(RwLock::new(HashSet::new())),
            NodeMode::Local,
            tmp.path().to_path_buf(),
            String::new(),
        );
        (state, tmp)
    }

    /// An unknown camera_id must 404 and leave NO trace — neither in the
    /// in-memory recording set nor in the persisted local_recording_state
    /// table.  Before the fix this returned 200 and wrote a junk row,
    /// letting a LAN client grow the (retention-free) table unboundedly.
    #[tokio::test]
    async fn toggle_recording_rejects_unknown_camera() {
        let (state, _tmp) = local_state_with_camera("nodeA_cam0");
        let filter = toggle_recording(state.clone());

        let resp = warp::test::request()
            .method("POST")
            .path("/api/cameras/ghost_cam/recording")
            .json(&serde_json::json!({ "recording": true }))
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 404, "unknown camera must 404");
        assert!(
            state.recording_state.read().unwrap().is_empty(),
            "unknown camera must not enter the in-memory recording set",
        );
        assert!(
            state.db.get_local_recording_state().unwrap().is_empty(),
            "unknown camera must not persist a local_recording_state row",
        );
    }

    /// A known camera toggles normally: 200, in-memory set updated, and
    /// the choice persisted so it survives a restart.
    #[tokio::test]
    async fn toggle_recording_enables_known_camera() {
        let (state, _tmp) = local_state_with_camera("nodeA_cam0");
        let filter = toggle_recording(state.clone());

        let resp = warp::test::request()
            .method("POST")
            .path("/api/cameras/nodeA_cam0/recording")
            .json(&serde_json::json!({ "recording": true }))
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        assert!(state.recording_state.read().unwrap().contains("nodeA_cam0"));
        assert_eq!(
            state
                .db
                .get_local_recording_state()
                .unwrap()
                .get("nodeA_cam0"),
            Some(&true),
            "known camera's recording choice must be persisted",
        );
    }

    /// Connected mode is a hard 409 regardless of camera validity — the
    /// route is disabled (CC's heartbeat reconciler owns the state).
    #[tokio::test]
    async fn toggle_recording_409s_in_connected_mode() {
        let (mut state, _tmp) = local_state_with_camera("nodeA_cam0");
        state.mode = NodeMode::Connected;
        let filter = toggle_recording(state.clone());

        let resp = warp::test::request()
            .method("POST")
            .path("/api/cameras/nodeA_cam0/recording")
            .json(&serde_json::json!({ "recording": true }))
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 409);
        // And it did NOT persist anything.
        assert!(state.db.get_local_recording_state().unwrap().is_empty());
    }
}
