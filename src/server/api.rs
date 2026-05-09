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
}

impl LocalApiState {
    pub fn new(
        dashboard: Dashboard,
        db: NodeDatabase,
        recording_state: Arc<RwLock<std::collections::HashSet<String>>>,
        mode: NodeMode,
        hls_base_dir: PathBuf,
    ) -> Self {
        Self {
            dashboard,
            db,
            recording_state,
            mode,
            hls_base_dir,
            uptime_start: std::time::Instant::now(),
            node_version: env!("CARGO_PKG_VERSION"),
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
        .and(with_state(state))
        .and_then(|camera_id: String, st: LocalApiState| async move {
            let result = crate::api::commands::take_snapshot(
                &camera_id,
                &st.hls_base_dir,
                &st.db,
            )
            .await;
            let reply: ApiReply = match result {
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
