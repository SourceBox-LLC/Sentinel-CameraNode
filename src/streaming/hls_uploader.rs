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
//! HLS Segment Uploader
//!
//! Watches HLS output directory and pushes new segments to the backend.
//! Maintains a rolling buffer locally while streaming to cloud.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::Semaphore;

use crate::api::ApiClient;
use crate::config::MotionConfig;
use crate::dashboard::{CameraStatus, Dashboard};
use crate::error::Result;
use crate::storage::NodeDatabase;
use super::motion_detector;
use super::segment_uploader::{SegmentUploader, UploadTask, UploaderConfig};

/// Motion event emitted when scene change exceeds the configured threshold.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MotionEvent {
    pub camera_id: String,
    pub score: u32,       // 0-100 (normalised from 0.0-1.0)
    pub timestamp: String, // ISO 8601
    pub segment_seq: u64,
}

/// Maximum concurrent segment uploads. Prevents unbounded task spawning
/// if uploads are slower than segment production (e.g., slow network).
/// 4 concurrent uploads is enough for 2s segments — even if each upload
/// takes 8 seconds, we still keep up.
static UPLOAD_SEMAPHORE: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(4)));

/// HLS Uploader configuration
#[derive(Debug, Clone)]
pub struct HlsUploaderConfig {
    /// Camera ID for this stream
    pub camera_id: String,
    /// Directory containing HLS files
    pub output_dir: PathBuf,
    /// Upload retry count
    pub retry_count: u32,
    /// Number of segments to keep locally after upload
    pub local_buffer_size: u32,
    /// True when the node is running in local-only mode.  When set,
    /// `push_segment` short-circuits and segments are never sent to a
    /// Command Center; the local recording write path runs as usual
    /// so playback through the local web UI still works.
    pub is_local: bool,
}

impl HlsUploaderConfig {
    pub fn new(camera_id: String, output_dir: PathBuf) -> Self {
        Self {
            camera_id,
            output_dir,
            retry_count: 3,
            local_buffer_size: 5, // Keep 5 segments locally (~5 seconds with 1s segments)
            is_local: false,
        }
    }

    /// Builder-style helper used by `node::runner::run_internal` when
    /// the node was configured for local-only mode.  Equivalent to
    /// `HlsUploaderConfig::new(...).with_local(true)` but avoids the
    /// extra binding at the call site.
    pub fn with_local(mut self, is_local: bool) -> Self {
        self.is_local = is_local;
        self
    }
}

/// HLS Segment Uploader
pub struct HlsUploader {
    config: HlsUploaderConfig,
    api_client: ApiClient,
    /// Track sequence number for ordering
    last_uploaded_seq: Arc<std::sync::atomic::AtomicU64>,
    /// Track whether codec has been detected
    codec_detected: Arc<std::sync::atomic::AtomicBool>,
    /// Which camera IDs are currently recording (shared with WS command handler)
    recording_state: Arc<RwLock<HashSet<String>>>,
    /// SQLite database for storing recorded segments
    db: NodeDatabase,
    /// Motion detection configuration
    motion_config: MotionConfig,
    /// Channel to send motion events to the WebSocket client
    motion_tx: tokio::sync::mpsc::Sender<MotionEvent>,
}

impl HlsUploader {
    /// Create a new HLS uploader
    pub fn new(
        config: HlsUploaderConfig,
        api_client: ApiClient,
        recording_state: Arc<RwLock<HashSet<String>>>,
        db: NodeDatabase,
        motion_config: MotionConfig,
        motion_tx: tokio::sync::mpsc::Sender<MotionEvent>,
    ) -> Self {
        Self {
            config,
            api_client,
            last_uploaded_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            codec_detected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recording_state,
            db,
            motion_config,
            motion_tx,
        }
    }

    /// Start with dashboard reporting (used by node runner).
    ///
    /// Uses polling instead of a file-system watcher. The `notify` crate's
    /// `ReadDirectoryChangesW` backend on Windows silently stops delivering
    /// events after ~95 segments, even though FFmpeg keeps producing them.
    /// Polling every second is simple, reliable, and adds at most 1s latency.
    ///
    /// `stall_flag` is raised when the pipeline has gone quiet for long
    /// enough that we suspect FFmpeg has wedged (not crashed — crashes
    /// are handled by the supervisor via `try_wait`).  The supervisor
    /// watches this flag and kills the child so the normal restart path
    /// fires.  Without it, a wedged-but-alive FFmpeg produces no segments
    /// indefinitely and the camera goes dark in the UI with no recovery.
    pub async fn start_with_dashboard(
        self,
        dash: Dashboard,
        camera_name: String,
        _camera_id: String,
        stall_flag: Arc<AtomicBool>,
    ) -> Result<()> {
        let poll_interval = tokio::time::Duration::from_secs(1);
        let mut seen: HashSet<String> = HashSet::new();
        let mut stale_cycles: u32 = 0;
        // Stall threshold in poll cycles (1s each).  10s already flips
        // the dashboard to Error; we wait until 20s before asking the
        // supervisor to kill FFmpeg so a brief V4L2 hiccup doesn't
        // trigger a spurious restart storm.
        const STALL_KILL_CYCLES: u32 = 20;

        // Orphan-sweep cadence.  Every `SWEEP_EVERY_CYCLES` polls (~60s
        // with the 1s poll interval) we reap stale `segment_*.ts` files
        // that the per-upload cleanup path missed.  Runs on a blocking
        // executor so the main loop doesn't stall on large directories.
        const SWEEP_EVERY_CYCLES: u32 = 60;
        // Keep roughly one minute of segments at 1s segment duration —
        // much larger than the `local_buffer_size=5` and
        // `hls_list_size=15` retention that FFmpeg + the uploader
        // already enforce, so this only trips when something has gone
        // wrong upstream.  Cap the disk-use tail at ~20 MB/camera.
        let sweep_keep_count: usize =
            (self.config.local_buffer_size as usize).saturating_add(60).max(30);
        let mut sweep_counter: u32 = 0;
        // Per-camera cooldown — this uploader is single-camera, so one Instant suffices
        let last_motion: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

        loop {
            // Scan the output directory for new .ts segments
            let mut new_segments: Vec<(u64, PathBuf)> = Vec::new();

            match std::fs::read_dir(&self.config.output_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();

                        if !name.ends_with(".ts") || seen.contains(&name) {
                            continue;
                        }

                        // Only enqueue if the file is large enough (FFmpeg finished writing)
                        if let Ok(meta) = std::fs::metadata(&path) {
                            if meta.len() >= 188 {
                                if let Some(seq) = extract_sequence_number(&name) {
                                    seen.insert(name);
                                    new_segments.push((seq, path));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read HLS directory: {}", e);
                }
            }

            // Process new segments in sequence order
            new_segments.sort_by_key(|(seq, _)| *seq);

            // Prune the seen set to prevent unbounded growth.
            // Keep only recent entries — old segments have been deleted from disk
            // and their names will never appear again.
            if seen.len() > 200 {
                let last_seq = self.last_uploaded_seq.load(std::sync::atomic::Ordering::SeqCst);
                let cutoff = last_seq.saturating_sub(50);
                seen.retain(|name| {
                    extract_sequence_number(name).map_or(true, |seq| seq >= cutoff)
                });
            }

            if new_segments.is_empty() {
                stale_cycles += 1;
                // Warn after 10 seconds of no new segments
                if stale_cycles == 10 {
                    dash.log_warn("No new segment in 10s — camera may have disconnected");
                    tracing::warn!(
                        "No new segments for camera {} in 10s",
                        self.config.camera_id
                    );
                    dash.update_camera_status(&camera_name, CameraStatus::Error(
                        "No segments (camera disconnected?)".into()
                    ));
                }
                // At STALL_KILL_CYCLES seconds of silence, raise the
                // stall flag so the supervisor kills FFmpeg.  We DON'T
                // re-raise on every subsequent cycle — the supervisor's
                // own 2s poll + kill + restart will clear this within a
                // few seconds, and we want the stall_cycles counter to
                // keep climbing so we can log escalating messages later
                // if the restart itself fails to recover.
                if stale_cycles == STALL_KILL_CYCLES {
                    dash.log_warn(format!(
                        "Pipeline wedged for {}s — asking supervisor to restart FFmpeg",
                        STALL_KILL_CYCLES
                    ));
                    tracing::warn!(
                        "Raising stall flag for camera {} after {}s of no segments",
                        self.config.camera_id,
                        STALL_KILL_CYCLES
                    );
                    stall_flag.store(true, Ordering::Relaxed);
                }
            } else {
                if stale_cycles >= 10 {
                    // We were stale but segments resumed — camera reconnected
                    dash.log_info("Segments resumed");
                    dash.update_camera_status(&camera_name, CameraStatus::Streaming);
                }
                stale_cycles = 0;
                // Any new segment implies the pipeline is healthy —
                // clear the stall flag so a historical kill request
                // doesn't fire after a natural recovery.
                stall_flag.store(false, Ordering::Relaxed);
            }

            for (seq, segment_path) in &new_segments {
                let seq = *seq;
                let segment_path = segment_path.clone();

                // Clone everything needed for the background task
                let uploader_config = UploaderConfig {
                    retry_count: self.config.retry_count,
                };
                let camera_id = self.config.camera_id.clone();
                let last_uploaded = self.last_uploaded_seq.clone();
                let codec_detected = self.codec_detected.clone();
                let api_client = self.api_client.clone();
                let cam_id_for_playlist = self.config.camera_id.clone();
                let output_dir = self.config.output_dir.clone();
                let dash = dash.clone();
                let camera_name = camera_name.to_string();
                let local_buffer_size = self.config.local_buffer_size;
                let hls_output_dir = self.config.output_dir.clone();
                let rec_state = self.recording_state.clone();
                let db = self.db.clone();
                let motion_cfg = self.motion_config.clone();
                let motion_tx = self.motion_tx.clone();
                let last_motion = last_motion.clone();

                // Spawn upload as a concurrent task so it doesn't block
                // the next segment. This prevents one slow upload from
                // stalling the entire pipeline. The semaphore limits
                // concurrent uploads to avoid unbounded task growth.
                let sem = UPLOAD_SEMAPHORE.clone();
                tokio::spawn(async move {
                    // Skip cameras the backend has suspended by plan cap.
                    // Pushing would return 402 on every segment and flood
                    // the log with non-retryable failures. The suspended
                    // set is kept fresh by the heartbeat loop; a plan
                    // upgrade clears it within one heartbeat (~30s) and
                    // pushes resume automatically on the next segment.
                    if dash.is_camera_suspended(&camera_id) {
                        return;
                    }

                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let uploader = SegmentUploader::new(uploader_config);
                    let file_size = tokio::fs::metadata(&segment_path).await.map(|m| m.len()).unwrap_or(0);

                    let task = UploadTask {
                        camera_id: camera_id.clone(),
                        segment_path: segment_path.clone(),
                        sequence: seq,
                    };

                    match uploader.push_segment(task, &api_client).await {
                        Ok(true) => {
                            let kb = file_size / 1024;
                            dash.record_upload(&camera_name, file_size);
                            dash.update_camera_status(&camera_name, CameraStatus::Streaming);
                            dash.log_debug(format!("Segment {:05} pushed ({} KB)", seq, kb));

                            // Codec detection (only first successful segment)
                            if !codec_detected.load(std::sync::atomic::Ordering::SeqCst) {
                                if let Ok(info) = super::codec_detector::detect_codec(&segment_path) {
                                    dash.set_codec(&camera_name, &info.video_codec, &info.audio_codec);
                                    if let Ok(_) = api_client.report_codec(&camera_id, &info.video_codec, &info.audio_codec).await {
                                        codec_detected.store(true, std::sync::atomic::Ordering::SeqCst);
                                        dash.log_info("Codec reported to cloud");
                                    }
                                }
                            }

                            // Motion detection (non-blocking, with per-camera cooldown)
                            if motion_cfg.enabled {
                                spawn_motion_detection(
                                    segment_path.clone(),
                                    camera_id.clone(),
                                    seq,
                                    &motion_cfg,
                                    motion_tx.clone(),
                                    last_motion.clone(),
                                    api_client.clone(),
                                );
                            }

                            // Playlist push (background, non-blocking).  Retry
                            // on transient failure — a single dropped push
                            // expires the backend's playlist cache and the
                            // browser gets 404 "Stream not started yet" even
                            // though fresh segments are still being uploaded.
                            // Matches the spirit of SegmentUploader's retry
                            // loop but with a shorter ceiling since playlists
                            // are cheap (<4 KB) and a new one will be written
                            // in another ~1 s anyway.
                            let api = api_client.clone();
                            let cam = cam_id_for_playlist;
                            // Clone so `output_dir` stays usable below
                            // (the local-recording branch reads
                            // `stream.m3u8` for #EXTINF parsing).
                            let dir = output_dir.clone();
                            tokio::spawn(async move {
                                let playlist_path = dir.join("stream.m3u8");
                                let content = match tokio::fs::read_to_string(&playlist_path).await {
                                    Ok(c) => c,
                                    Err(_) => return,
                                };
                                if !content.starts_with("#EXTM3U") || !content.contains("#EXTINF") {
                                    return;
                                }
                                const MAX_ATTEMPTS: u32 = 3;
                                let mut delay_ms: u64 = 250;
                                for attempt in 1..=MAX_ATTEMPTS {
                                    match api.update_playlist(&cam, &content).await {
                                        Ok(_) => return,
                                        Err(e) => {
                                            if attempt == MAX_ATTEMPTS {
                                                tracing::warn!(
                                                    "Playlist push failed after {} attempts: {}",
                                                    MAX_ATTEMPTS, e,
                                                );
                                            } else {
                                                tracing::debug!(
                                                    "Playlist push attempt {} failed ({}); retrying in {} ms",
                                                    attempt, e, delay_ms,
                                                );
                                                tokio::time::sleep(
                                                    std::time::Duration::from_millis(delay_ms),
                                                ).await;
                                                delay_ms = (delay_ms * 2).min(1000);
                                            }
                                        }
                                    }
                                }
                            });

                            last_uploaded.store(seq, std::sync::atomic::Ordering::SeqCst);

                            // Per-task cleanup: save THIS segment to the DB
                            // (if recording) and delete THIS segment from
                            // disk.  Touch nothing else.
                            //
                            // Race history (fixed in v0.1.45): the previous
                            // version walked back through `[seq-25, seq-5)`
                            // and deleted every segment in that window —
                            // ostensibly to maintain a 5-segment local
                            // buffer and reap orphans missed by earlier
                            // tasks.  Safe in isolation, catastrophic with
                            // concurrent uploads.
                            //
                            // The upload pool runs up to 4 segments in
                            // parallel (UPLOAD_SEMAPHORE).  Tasks complete
                            // in roughly-but-not-exactly seq order, and a
                            // task for seq N finishing first would delete
                            // segments still being uploaded by tasks for
                            // seqs N-25 .. N-5.  Pi log captured the exact
                            // pattern — bursts of ~20 sequential
                            // `No such file or directory (os error 2)`
                            // errors right at the buffer boundary,
                            // recovering as soon as the racing task
                            // finished its cleanup.
                            //
                            // Delete-my-own-segment-only is race-free: no
                            // task touches another task's file.  Anything
                            // that slips through (e.g. a task that errored
                            // before reaching this point) is reaped by the
                            // orphan sweeper below on its 60s cadence,
                            // which is the appropriate scope for "files
                            // nobody currently owns" cleanup.
                            //
                            // _ used on local_buffer_size below — kept in
                            // the config for API compatibility but no
                            // longer drives behaviour.
                            let _ = local_buffer_size;
                            let _ = hls_output_dir;

                            let is_recording = rec_state.read()
                                .map(|s| s.contains(&camera_id))
                                .unwrap_or(false);

                            // Safety floor: if the host disk is critically
                            // low (set by storage::stats::collect from the
                            // heartbeat task), skip recording writes
                            // entirely.  We still delete the source file —
                            // the live HLS rolling buffer must keep
                            // rotating or FFmpeg fills the disk anyway.
                            // What we drop is just the durable archive
                            // copy.  The retention loop will free up DB
                            // space on its 5-min cadence.
                            let paused = crate::storage::should_pause_recording();

                            if is_recording && !paused {
                                let today = chrono::Utc::now()
                                    .format("%Y-%m-%d").to_string();
                                if let Ok(data) = tokio::fs::read(&segment_path).await {
                                    // Parse #EXTINF for this segment from
                                    // stream.m3u8 so the dynamic playback
                                    // playlist (Phase B local web UI) has
                                    // accurate per-segment durations.
                                    // FFmpeg's HLS muxer with target=1s
                                    // mostly produces ~1.000s segments but
                                    // boundaries vary; fall back to 1000ms
                                    // when the playlist isn't readable yet
                                    // (e.g. supervisor just restarted).
                                    let duration_ms = read_segment_duration_ms(
                                        &output_dir,
                                        seq,
                                    )
                                    .await
                                    .unwrap_or(1000);
                                    let _ = db.save_recording_segment(
                                        &camera_id, seq, &today, &data, duration_ms,
                                    );
                                }
                            }

                            // Deliberately DO NOT delete the segment here in
                            // either mode.  Letting the orphan sweeper own
                            // all cleanup (60 s cadence, keeps ≥30 newest
                            // segments, ~12-20 MB per camera) is the only
                            // way to keep the snapshot grab path reliable —
                            // `find_latest_segment` reads the second-to-
                            // latest `.ts` from disk, and an immediate
                            // post-push delete used to race the WS
                            // `take_snapshot` command in Connected mode.
                            //
                            // The Local-mode gate was the original fix for
                            // this in v0.1.50, but the same race lived in
                            // Connected mode the whole time — it surfaced as
                            // "No segments found for camera <id>" when an
                            // operator clicked Snapshot in Command Center
                            // between segment writes.  Removing the gate in
                            // v0.1.56 makes both modes equally reliable.
                            //
                            // Slow-uplink scenarios are unaffected: in those
                            // cases the upload task is blocked waiting on
                            // reqwest, so the segment would have stayed on
                            // disk anyway — the delete inside this task
                            // never fires until the upload completes.
                        }
                        Ok(false) => {
                            tracing::debug!("Skipped segment {} (too small)", seq);
                        }
                        Err(e) => {
                            dash.log_warn(format!("Segment {} push failed: {}", seq, e));
                        }
                    }
                });
            }

            // Periodic orphan sweep — see SWEEP_EVERY_CYCLES comment above.
            sweep_counter = sweep_counter.wrapping_add(1);
            if sweep_counter >= SWEEP_EVERY_CYCLES {
                sweep_counter = 0;
                let out_dir = self.config.output_dir.clone();
                let cam_name = camera_name.clone();
                let d = dash.clone();
                let keep = sweep_keep_count;
                tokio::task::spawn_blocking(move || {
                    match sweep_orphan_segments(&out_dir, keep) {
                        Ok((n, bytes)) if n > 0 => {
                            d.log_warn(format!(
                                "Orphan sweep ({}): removed {} stale segment(s), freed {} KB",
                                cam_name,
                                n,
                                bytes / 1024,
                            ));
                            tracing::warn!(
                                "Orphan sweep for {}: removed {} stale segment(s), freed {} KB",
                                cam_name,
                                n,
                                bytes / 1024,
                            );
                        }
                        Ok(_) => {
                            // Nothing to do — the normal cleanup paths
                            // kept the directory tidy.
                        }
                        Err(e) => {
                            tracing::debug!("Orphan sweep failed for {}: {}", cam_name, e);
                        }
                    }
                });
            }

            tokio::time::sleep(poll_interval).await;
        }
    }
}

/// Spawn a background task that runs FFmpeg scene-change detection on a
/// segment and, if motion exceeds the threshold and the per-camera cooldown
/// has elapsed, sends the event to the WebSocket client.
fn spawn_motion_detection(
    segment_path: PathBuf,
    camera_id: String,
    seq: u64,
    motion_cfg: &MotionConfig,
    _tx: tokio::sync::mpsc::Sender<MotionEvent>,
    last_motion: Arc<Mutex<Option<Instant>>>,
    api_client: ApiClient,
) {
    let threshold = motion_cfg.threshold;
    let cooldown = std::time::Duration::from_secs(motion_cfg.cooldown_secs);

    tokio::spawn(async move {
        if let Some(score) = motion_detector::detect_motion(&segment_path, threshold).await {
            // Check cooldown before sending (scoped to drop guard before await).
            // Recover from a poisoned lock rather than panicking — the protected
            // state is just a timestamp; if a prior task died mid-critical-section
            // the worst outcome is one extra motion event fires, not data loss.
            // Without this, a single panic inside the critical section wedges all
            // future motion detection on this node until the daemon restarts.
            let now = Instant::now();
            {
                let mut guard = last_motion
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(last) = *guard {
                    if now.duration_since(last) < cooldown {
                        return;
                    }
                }
                *guard = Some(now);
            }

            let score_int = (score * 100.0).round() as u32;
            let timestamp = chrono::Utc::now().to_rfc3339();

            // Deliver via HTTP POST (reliable, works without WebSocket)
            if let Err(e) = api_client.report_motion(
                &camera_id, score_int, &timestamp, seq,
            ).await {
                tracing::warn!("Motion HTTP report failed: {}", e);
            }
        }
    });
}

/// Extract sequence number from segment filename
fn extract_sequence_number(filename: &str) -> Option<u64> {
    // Format: segment_00001.ts
    let parts: Vec<&str> = filename.split('_').collect();
    if parts.len() != 2 {
        return None;
    }

    let num_part = parts[1].trim_end_matches(".ts");
    num_part.parse().ok()
}

/// Reap `segment_*.ts` files from a camera's HLS output directory.
///
/// This is now the **sole** cleanup path for HLS segments — we removed
/// FFmpeg's `-hls_flags delete_segments` in v0.1.17 because on Windows its
/// rotation-delete raced Windows Defender / NTFS lazy-close and logged
/// `failed to delete old segment ...` on every rotation. Running cleanup
/// from our own process, on a 60-cycle (~60s) cadence, avoids the race:
/// transient handles have long since closed by the time the sweeper runs.
///
/// Keeps the `keep_count` segments with the **highest sequence numbers**
/// (not mtime — sequence is monotonic, filesystem timestamps on FAT / SD
/// Read the per-segment EXTINF duration from a freshly-written
/// `stream.m3u8` and return it in milliseconds.
///
/// The HLS muxer writes lines like
///   #EXTINF:1.001000,
///   segment_00042.ts
/// for every segment in the rolling window.  We find the line ending
/// in `segment_<seq>.ts` and pull the `#EXTINF:` value above it.
///
/// Returns None when the playlist isn't readable yet, when the segment
/// isn't in the playlist (uploader's poll loop can win the race against
/// the muxer's atomic playlist replace), or when the EXTINF parse fails.
/// Caller falls back to a configured default (1000 ms = 1.0 s, which is
/// the muxer's target_duration).
pub(crate) async fn read_segment_duration_ms(
    output_dir: &std::path::Path,
    seq: u64,
) -> Option<u32> {
    let playlist_path = output_dir.join("stream.m3u8");
    let body = tokio::fs::read_to_string(&playlist_path).await.ok()?;
    let needle = format!("segment_{:05}.ts", seq);
    let mut last_extinf: Option<f64> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#EXTINF:") {
            // Format: `#EXTINF:1.000000,` (the comma terminates the
            // duration; some encoders also append a title after it).
            let value = rest.split(',').next()?.trim();
            last_extinf = value.parse::<f64>().ok();
        } else if trimmed.ends_with(&needle) {
            return last_extinf.map(|s| (s * 1000.0).round() as u32);
        }
    }
    None
}

/// cards can skew by seconds) and removes the rest.  Returns
/// `(files_removed, bytes_freed)`.
///
/// With a 1s segment cadence and `keep_count ≈ 30`, worst-case disk use
/// between sweeps is ~30 × ~400 KB = ~12 MB per camera — well below the
/// bounds that motivated the original sweep on Pi 4s with flaky uplinks.
pub(crate) fn sweep_orphan_segments(
    output_dir: &std::path::Path,
    keep_count: usize,
) -> std::io::Result<(usize, u64)> {
    let mut segments: Vec<(u64, std::path::PathBuf, u64)> = Vec::new();
    for entry in std::fs::read_dir(output_dir)?.flatten() {
        // Skip non-UTF8 filenames explicitly rather than mangling them
        // through `to_string_lossy`.  `to_string_lossy` would turn a
        // non-UTF8 byte into `�`, which then silently fails the
        // `segment_` prefix match — meaning a weirdly-named orphan
        // (shouldn't exist; FFmpeg only writes ASCII) would never be
        // reaped.  Being explicit costs nothing and keeps the sweeper
        // honest: "I only touch files I fully understand."
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with("segment_") || !name.ends_with(".ts") {
            continue;
        }
        let Some(seq) = extract_sequence_number(name) else {
            continue;
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        segments.push((seq, entry.path(), size));
    }

    if segments.len() <= keep_count {
        return Ok((0, 0));
    }

    // Sort by sequence ASC so the tail is the newest `keep_count`
    // segments.  Drop everything before the tail.
    segments.sort_by_key(|(seq, _, _)| *seq);
    let remove_count = segments.len() - keep_count;
    let mut freed = 0u64;
    let mut removed = 0usize;
    for (_, path, size) in segments.into_iter().take(remove_count) {
        if std::fs::remove_file(&path).is_ok() {
            freed += size;
            removed += 1;
        }
    }
    Ok((removed, freed))
}

// File watcher (notify crate) was removed because ReadDirectoryChangesW
// on Windows silently stops delivering events after ~95 segments.
// Replaced by simple 1-second polling in the upload loop above.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_sequence_number() {
        assert_eq!(extract_sequence_number("segment_00001.ts"), Some(1));
        assert_eq!(extract_sequence_number("segment_00042.ts"), Some(42));
        assert_eq!(extract_sequence_number("segment_12345.ts"), Some(12345));
        assert_eq!(extract_sequence_number("invalid.ts"), None);
        assert_eq!(extract_sequence_number("segment_.ts"), None);
        assert_eq!(extract_sequence_number("playlist.m3u8"), None);
    }

    #[test]
    fn test_hls_uploader_config() {
        let config = HlsUploaderConfig::new("camera_123".into(), PathBuf::from("/data/hls/camera_123"));
        assert_eq!(config.camera_id, "camera_123");
        assert_eq!(config.local_buffer_size, 5);
        assert_eq!(config.retry_count, 3);
        // is_local defaults to false (Connected mode is the back-compat
        // default — matches NodeMode::default).
        assert!(!config.is_local);
    }

    #[test]
    fn test_hls_uploader_config_with_local_builder() {
        let config = HlsUploaderConfig::new(
            "camera_456".into(),
            PathBuf::from("/data/hls/camera_456"),
        )
        .with_local(true);
        assert!(config.is_local);
        assert_eq!(config.camera_id, "camera_456");
    }

    // ── Orphan sweeper regression tests ───────────────────────────────
    //
    // These lock in the disk-full recovery path added in v0.1.16 after
    // a Pi 4 deployment filled its SD card with segments the inline
    // upload cleanup had missed.  See `docs/runbooks/video-not-showing.md`.

    fn write_segment(dir: &std::path::Path, seq: u64, bytes: &[u8]) {
        let path = dir.join(format!("segment_{:05}.ts", seq));
        std::fs::write(&path, bytes).expect("write segment");
    }

    #[test]
    fn sweep_keeps_newest_segments_by_sequence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for seq in 1..=20u64 {
            write_segment(tmp.path(), seq, &[0u8; 1024]);
        }
        let (removed, freed) = sweep_orphan_segments(tmp.path(), 5).expect("sweep ok");
        assert_eq!(removed, 15, "should remove 15 oldest of 20");
        assert_eq!(freed, 15 * 1024, "should report bytes freed");

        // Only segments 16..=20 should remain.
        let mut remaining: Vec<u64> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                extract_sequence_number(&name)
            })
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec![16, 17, 18, 19, 20]);
    }

    #[test]
    fn sweep_noop_when_below_keep_count() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for seq in 1..=3u64 {
            write_segment(tmp.path(), seq, b"x");
        }
        let (removed, freed) = sweep_orphan_segments(tmp.path(), 10).expect("sweep ok");
        assert_eq!(removed, 0);
        assert_eq!(freed, 0);
        let count = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 3, "no segments should be deleted");
    }

    #[test]
    fn sweep_ignores_non_segment_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Real segments
        for seq in 1..=10u64 {
            write_segment(tmp.path(), seq, b"ts");
        }
        // Files the sweeper must not touch.
        std::fs::write(tmp.path().join("stream.m3u8"), "#EXTM3U\n").unwrap();
        std::fs::write(tmp.path().join("README"), "hi").unwrap();
        std::fs::write(tmp.path().join("segment_bogus.ts"), "x").unwrap();

        let (removed, _) = sweep_orphan_segments(tmp.path(), 3).expect("sweep ok");
        assert_eq!(removed, 7, "only segment_NNNNN.ts files should be reaped");

        // Non-segment files must still exist.
        assert!(tmp.path().join("stream.m3u8").exists());
        assert!(tmp.path().join("README").exists());
        assert!(tmp.path().join("segment_bogus.ts").exists());
    }

    #[test]
    fn sweep_handles_nonexistent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let result = sweep_orphan_segments(&missing, 5);
        assert!(result.is_err(), "should surface the io::Error to caller");
    }
}
