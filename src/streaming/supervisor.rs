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

//! FFmpeg supervisor.
//!
//! Wraps a per-camera `HlsGenerator` with a loop that:
//!   * polls FFmpeg's exit status every `POLL_INTERVAL`
//!   * when the child exits, respawns it with exponential backoff
//!     (1s, 2s, 4s, …, capped at 30s — matches the WS reconnect backoff)
//!   * bails (→ `Failed`) after too many restarts in a 60s window, so a
//!     permanently broken camera / FFmpeg config / disk-full situation
//!     doesn't spin forever
//!   * watches the uploader's `stall_flag` and kills a wedged-but-alive
//!     FFmpeg so it routes through the same restart path.  Without this,
//!     a pipeline that stops producing segments but keeps the child
//!     process alive (V4L2 deadlock, thermal throttle below real-time,
//!     USB bandwidth starvation) would sit wedged forever and `try_wait`
//!     would never return an exit status to trip the restart loop.
//!   * pushes `CameraStatus::Streaming / Restarting / Failed` into the
//!     Dashboard so the WS + HTTP heartbeats report real pipeline state
//!     instead of the old hardcoded `"streaming"`.
//!
//! Prior to this supervisor, an FFmpeg crash (e.g. a disk-full errno -28,
//! a closed V4L2 fd, a segment-writer failure) would silently leave the
//! camera offline from the browser's point of view while the node still
//! reported `status: streaming` in every heartbeat. The backend MCP
//! tools would then tell users "update CloudNode to latest version"
//! when the real failure was upstream in FFmpeg.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dashboard::{CameraStatus, Dashboard};
use crate::streaming::{HlsGenerator, HlsGeneratorConfig};

/// How often we poll FFmpeg's child process for exit.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Starting backoff after the first crash.
const BASE_BACKOFF: Duration = Duration::from_secs(1);

/// Upper bound on backoff between restart attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Restart-count sliding window.
const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// If the pipeline crashes more than this many times inside
/// `RESTART_WINDOW`, stop trying and mark it `Failed`.
const MAX_RESTARTS_IN_WINDOW: u32 = 5;

/// How long the pipeline has to be healthy before we consider the
/// failure sequence "over" and reset the backoff.
const HEALTHY_RESET_THRESHOLD: Duration = Duration::from_secs(60);

/// A way to start FFmpeg — either a real capture device or a
/// synthetic test pattern (used as a fallback on machines without
/// a working webcam so dev / CI can still exercise the pipeline).
#[derive(Clone)]
pub enum PipelineSource {
    Device(String),
    /// (width, height, fps)
    TestPattern(u32, u32, u32),
}

/// Static config the supervisor needs for the lifetime of a camera.
pub struct SupervisorConfig {
    pub hls_config: HlsGeneratorConfig,
    /// Try this source first on every (re)start.
    pub primary: PipelineSource,
    /// If `primary` fails on the VERY FIRST attempt, fall through to
    /// this. Useful for "try real camera, fall back to test pattern".
    /// Ignored on subsequent restarts — once we've committed to a
    /// source that works, we stay with it.
    pub fallback: Option<PipelineSource>,
    pub camera_name: String,
    pub camera_id: String,
    /// Shared with the uploader.  The uploader raises this flag after
    /// ~20s of no new segments; the supervisor sees it, kills FFmpeg,
    /// and the existing crash path restarts it.
    pub stall_flag: Arc<AtomicBool>,
    /// Shared with the uploader.  Bumped by the supervisor on EVERY
    /// FFmpeg (re)start, right after cleanup() wipes the directory —
    /// ground truth for the uploader to clear its seen/dedup state.
    /// Replaces inference: the seq-regression heuristic has a dead
    /// band when the previous run produced ≤ RESET_GAP segments, in
    /// which a crash-looping camera's fresh low-seq files stayed in
    /// `seen` and were never uploaded or archived again.
    pub restart_epoch: Arc<AtomicU64>,
}

/// Supervise a single camera's HLS pipeline.
///
/// Owns a fresh `HlsGenerator` per run. Never returns under normal
/// operation — the caller either `abort()`s the task on shutdown or
/// flips `stop_flag` so we break out of the poll loop cleanly.
pub async fn supervise_hls(
    cfg: SupervisorConfig,
    dash: Dashboard,
    stop_flag: Arc<AtomicBool>,
) {
    let SupervisorConfig {
        hls_config,
        primary,
        fallback,
        camera_name,
        camera_id,
        stall_flag,
        restart_epoch,
    } = cfg;

    // Which source we're currently trying. Starts as `primary`; if that
    // fails the very first attempt, we switch to `fallback` (once).
    let mut active_source = primary;
    let mut fallback_consumed = false;

    let mut backoff = BASE_BACKOFF;
    // Timestamps of recent crashes; anything older than RESTART_WINDOW
    // gets popped off the front so bursts after long healthy periods
    // don't accumulate.
    let mut crash_history: VecDeque<Instant> = VecDeque::new();

    'outer: loop {
        if stop_flag.load(Ordering::Relaxed) {
            return;
        }

        // ── Build a fresh generator and start FFmpeg ───────────────────
        let mut generator = match HlsGenerator::new(hls_config.clone()) {
            Ok(g) => g,
            Err(e) => {
                let err = format!("HlsGenerator::new failed: {}", e);
                dash.log_error(format!("[{}] supervisor: {}", camera_name, err));
                dash.update_camera_status_by_id(
                    &camera_id,
                    CameraStatus::Failed { last_error: err },
                );
                return;
            }
        };

        let start_result = start_with(&mut generator, &active_source);

        // Clear any stall flag left over from a previous incarnation —
        // otherwise the uploader's own 20s clock (still running) would
        // race against a flag we meant for the prior FFmpeg.
        stall_flag.store(false, Ordering::Relaxed);
        // Signal the uploader that the directory was wiped and FFmpeg's
        // segment counter restarted from 0 — it clears its dedup state
        // on the next scan.  Bumped on every attempt (success or fail):
        // cleanup() ran either way, so any names the uploader remembers
        // refer to files that no longer exist.
        restart_epoch.fetch_add(1, Ordering::SeqCst);

        let encoder = match &start_result {
            Ok(enc) => enc.clone(),
            Err(_) => String::new(),
        };

        if let Err(e) = start_result {
            let err = annotate_with_disk_state(
                format!("FFmpeg start failed: {}", e),
                &hls_config.output_dir,
            );
            dash.log_warn(format!("[{}] supervisor: {}", camera_name, err));

            // One-shot fallback on the very first attempt: if primary
            // failed and a fallback is configured, switch and try once
            // before counting this as a "real" crash.
            if !fallback_consumed && crash_history.is_empty() {
                if let Some(fb) = fallback.clone() {
                    dash.log_info(format!(
                        "[{}] supervisor: trying fallback source",
                        camera_name
                    ));
                    active_source = fb;
                    fallback_consumed = true;
                    // Immediately retry, no backoff — this is the
                    // probe path, not a restart.
                    continue 'outer;
                }
            }

            let should_restart = record_crash_and_maybe_bail(
                &mut crash_history,
                &camera_id,
                &camera_name,
                &err,
                &dash,
            );
            if !should_restart {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue 'outer;
        }

        // FFmpeg is up. Flip the dashboard to Streaming.
        fallback_consumed = true; // don't downgrade to fallback after a success
        dash.update_camera_status_by_id(&camera_id, CameraStatus::Streaming);
        let alive_since = Instant::now();

        // Surface the encoder choice to the TUI on every (re)start.
        // Operators debugging thermal / CPU issues need this visible at
        // a glance — it was previously only in the tracing log which
        // nobody sees unless they tail journalctl.  Warn loudly when
        // software encoding is selected on ARM (Raspberry Pi etc.):
        // libx264 + two 720p30 streams can easily pin a Pi 4 CPU into
        // thermal throttling, which then surfaces as exactly the wedge
        // this supervisor is designed to recover from.
        if !encoder.is_empty() {
            dash.log_info(format!(
                "[{}] FFmpeg running (encoder: {})",
                camera_name, encoder
            ));
            if encoder == "libx264" && cfg!(target_arch = "aarch64") {
                dash.log_warn(format!(
                    "[{}] Software encoder on ARM — high CPU; consider \
                     passing /dev/video10 to the container for h264_v4l2m2m",
                    camera_name
                ));
            }
        }

        // ── Poll until FFmpeg exits or we're told to stop ──────────────
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                // Best-effort kill before return so we surface any kill
                // error to the log.  `HlsGenerator`'s Drop impl also
                // calls `stop()` as a fallback, but it swallows errors
                // there — we'd rather see them in tracing at shutdown.
                let _ = generator.stop();
                return;
            }

            tokio::time::sleep(POLL_INTERVAL).await;

            // If the uploader detected a wedge, kill FFmpeg so the
            // next `check_process` call returns an exit status and
            // routes through the existing restart path.  We swap
            // rather than load to ensure a single kill even if the
            // uploader somehow re-raises before the child dies.
            if stall_flag.swap(false, Ordering::Relaxed) {
                dash.log_warn(format!(
                    "[{}] supervisor: pipeline stalled — killing FFmpeg",
                    camera_name
                ));
                tracing::warn!(
                    "Supervisor killing wedged FFmpeg for camera {}",
                    camera_id
                );
                let _ = generator.stop();
                // Fall through to check_process — it'll pick up the
                // exit status on this or the next poll tick.
            }

            let Some(status) = generator.check_process() else {
                // Still running — keep polling.
                continue;
            };

            let alive_for = alive_since.elapsed();
            let err = annotate_with_disk_state(
                format!(
                    "FFmpeg exited with {} after running {}s",
                    status,
                    alive_for.as_secs()
                ),
                &hls_config.output_dir,
            );
            dash.log_warn(format!("[{}] {}", camera_name, err));

            // If the pipeline stayed healthy long enough, treat this
            // crash as a new failure sequence — reset backoff & window.
            if alive_for >= HEALTHY_RESET_THRESHOLD {
                crash_history.clear();
                backoff = BASE_BACKOFF;
            }

            let should_restart = record_crash_and_maybe_bail(
                &mut crash_history,
                &camera_id,
                &camera_name,
                &err,
                &dash,
            );
            if !should_restart {
                return;
            }

            // Flip to Restarting BEFORE the sleep so the very next
            // heartbeat surfaces the real state (instead of waiting
            // until we've actually respawned).
            let attempt = crash_history.len() as u32;
            dash.update_camera_status_by_id(
                &camera_id,
                CameraStatus::Restarting {
                    attempt,
                    last_error: err.clone(),
                },
            );
            dash.log_info(format!(
                "[{}] supervisor: restarting in {}s (attempt {})",
                camera_name,
                backoff.as_secs(),
                attempt
            ));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue 'outer;
        }
    }
}

fn start_with(
    generator: &mut HlsGenerator,
    source: &PipelineSource,
) -> crate::error::Result<String> {
    match source {
        PipelineSource::Device(path) => generator.start_from_device(path),
        PipelineSource::TestPattern(w, h, fps) => generator.start_from_frames(*w, *h, *fps),
    }
}

/// Report free bytes on the filesystem containing `path`.
///
/// Returns `None` if the query fails or isn't implemented for this OS.
/// Used by the supervisor to annotate crash reports when the box is
/// almost certainly out of disk — which on a Pi running for days with
/// a flaky uplink is the most common cause of an FFmpeg bounce loop
/// that nothing in the stack can recover from.
#[cfg(target_os = "linux")]
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes only to the buffer we provide; the path
    // is a valid NUL-terminated C string we just constructed.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    Some((buf.f_bavail as u64).saturating_mul(buf.f_bsize as u64))
}

#[cfg(not(target_os = "linux"))]
fn free_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

/// If `output_dir`'s filesystem has less than this many bytes free, we
/// annotate supervisor crash messages with a "(disk exhausted)" tag so
/// the dashboard, heartbeats, and `get_node` MCP tool all surface the
/// real cause without requiring SSH.  Chosen small enough that a
/// healthy Pi never trips it (a few hours of HLS segments is ~1 GB)
/// but large enough to catch the tail of the fill.
const LOW_DISK_THRESHOLD_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// Prepend a "(disk exhausted)" tag to `err` if the output dir's
/// filesystem is below `LOW_DISK_THRESHOLD_BYTES`.  No-op otherwise.
fn annotate_with_disk_state(err: String, output_dir: &std::path::Path) -> String {
    match free_bytes(output_dir) {
        Some(free) if free < LOW_DISK_THRESHOLD_BYTES => {
            let mib = free / (1024 * 1024);
            format!("(disk exhausted: {} MiB free) {}", mib, err)
        }
        _ => err,
    }
}

/// Append a crash to the sliding window. Returns `false` (→ bail, mark
/// Failed) if the window already has too many crashes, else `true`.
fn record_crash_and_maybe_bail(
    crash_history: &mut VecDeque<Instant>,
    camera_id: &str,
    camera_name: &str,
    last_error: &str,
    dash: &Dashboard,
) -> bool {
    let now = Instant::now();
    crash_history.push_back(now);
    // Drop crashes older than the window.
    while let Some(front) = crash_history.front() {
        if now.duration_since(*front) > RESTART_WINDOW {
            crash_history.pop_front();
        } else {
            break;
        }
    }

    if crash_history.len() as u32 > MAX_RESTARTS_IN_WINDOW {
        dash.log_error(format!(
            "[{}] supervisor: {} restarts in {}s — giving up",
            camera_name,
            crash_history.len(),
            RESTART_WINDOW.as_secs()
        ));
        dash.update_camera_status_by_id(
            camera_id,
            CameraStatus::Failed {
                last_error: last_error.to_string(),
            },
        );
        return false;
    }
    true
}
