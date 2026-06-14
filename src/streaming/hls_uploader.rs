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
//! HLS Segment Uploader
//!
//! Watches HLS output directory and pushes new segments to the backend.
//! Maintains a rolling buffer locally while streaming to cloud.

use std::collections::{HashSet, VecDeque};
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

// `MotionEvent` struct lived here for the never-implemented WS
// forwarding path described in the comments at runner.rs:334 and
// websocket.rs:108.  v0.1.61 removed the channel + sender + receiver;
// the struct itself stayed orphaned until v0.1.63 caught it during the
// docs-accuracy review.  Motion events are now delivered HTTP-only
// inside `spawn_motion_detection` via `ApiClient::report_motion`
// (`camera_id`, `score`, `timestamp`, `segment_seq` as positional
// args), so the struct is no longer needed.

/// Maximum concurrent segment uploads **per camera**.  Bounds task
/// growth when uploads are slower than segment production (slow uplink).
/// 4 in-flight uploads keeps up with 1 s segments even if each push
/// takes several seconds.
///
/// This used to be a single process-global `Semaphore::new(4)` shared
/// across every camera — which meant one camera on a backed-up uplink
/// could hold all 4 permits and stall *every other* camera's pushes
/// (a multi-camera starvation bug that only bites at the exact scale
/// moment you add a second camera).  Each `HlsUploader` (one per
/// camera) now owns its own semaphore, so a slow camera only throttles
/// itself.
const MAX_CONCURRENT_UPLOADS_PER_CAMERA: usize = 4;

/// Spawn a detached task whose panics are logged instead of silently
/// swallowed.
///
/// The upload hot loop fires off background tasks (segment push, playlist
/// push, motion detection) with `tokio::spawn` and never awaits the
/// `JoinHandle` — fire-and-forget.  Tokio's default behaviour for a
/// panicking detached task is to abort just that task; the panic message
/// goes to the default panic hook (stderr), which on a TUI build is
/// painted over by the dashboard and effectively invisible.  The
/// operator-visible symptom is a silently dropped segment / skipped
/// playlist push / missed motion event with no trail.
///
/// Wrapping the future in `catch_unwind` turns a panic into a
/// `tracing::error!` tagged with the task label, so it lands in the
/// SQLite log buffer the dashboard renders + survives restarts.  Normal
/// (non-panicking) completion is unaffected.
fn spawn_logged<F>(label: &'static str, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use futures_util::FutureExt;
    tokio::spawn(async move {
        if std::panic::AssertUnwindSafe(fut).catch_unwind().await.is_err() {
            tracing::error!(
                "background task '{}' panicked — its segment work was dropped",
                label,
            );
        }
    });
}

/// HLS Uploader configuration
#[derive(Debug, Clone)]
pub struct HlsUploaderConfig {
    /// Camera ID for this stream
    pub camera_id: String,
    /// Directory containing HLS files
    pub output_dir: PathBuf,
    /// Upload retry count
    pub retry_count: u32,
    /// Number of segments to keep locally after upload.  Feeds the
    /// orphan sweeper's keep window (sweep_keep_count = local_buffer_size + 60).
    pub local_buffer_size: u32,
}

impl HlsUploaderConfig {
    pub fn new(camera_id: String, output_dir: PathBuf) -> Self {
        Self {
            camera_id,
            output_dir,
            retry_count: 3,
            local_buffer_size: 5, // Keep 5 segments locally (~5 seconds with 1s segments)
        }
    }
}

// Pre-v0.1.62 `HlsUploaderConfig` carried an `is_local: bool` field
// + a `with_local()` builder.  Its only consumer was the per-task
// delete gate in the upload hot loop, which we removed in v0.1.56
// (the orphan sweeper owns all cleanup in both modes now to keep
// the snapshot grab path reliable).  Field + builder retired in
// v0.1.62 — Local-mode behaviour is now distinguished entirely by
// `ApiClient::is_local()` (segment-push short-circuit) and
// `NodeMode` reads in `runner.rs` (heartbeat/WS spawn gating).

/// HLS Segment Uploader
pub struct HlsUploader {
    config: HlsUploaderConfig,
    api_client: ApiClient,
    /// Track whether codec has been detected
    codec_detected: Arc<std::sync::atomic::AtomicBool>,
    /// Which camera IDs are currently recording (shared with WS command handler)
    recording_state: Arc<RwLock<HashSet<String>>>,
    /// SQLite database for storing recorded segments
    db: NodeDatabase,
    /// Motion detection configuration
    motion_config: MotionConfig,
    /// Per-camera concurrent-upload limiter.  One semaphore per uploader
    /// instance (per camera) so a slow uplink on this camera can't starve
    /// other cameras' uploads — see MAX_CONCURRENT_UPLOADS_PER_CAMERA.
    upload_semaphore: Arc<Semaphore>,
}

impl HlsUploader {
    /// Create a new HLS uploader.
    ///
    /// Note (v0.1.61): the `motion_tx` parameter was removed.  Motion
    /// events have only ever been delivered via the HTTP `report_motion`
    /// path inside `spawn_motion_detection`; the mpsc channel that used
    /// to forward them to the WS task was orphaned plumbing that the
    /// next refactor would have tripped over.
    pub fn new(
        config: HlsUploaderConfig,
        api_client: ApiClient,
        recording_state: Arc<RwLock<HashSet<String>>>,
        db: NodeDatabase,
        motion_config: MotionConfig,
    ) -> Self {
        Self {
            config,
            api_client,
            codec_detected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recording_state,
            db,
            motion_config,
            upload_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS_PER_CAMERA)),
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
        restart_epoch: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<()> {
        let poll_interval = tokio::time::Duration::from_secs(1);
        // `seen` dedupes already-processed segment files by name; `seen_order`
        // records insertion order so the prune evicts the OLDEST-discovered
        // names first. Pruning by recency (not by sequence number) is what
        // keeps the uploader correct across FFmpeg restarts: a restart resets
        // FFmpeg's segment counter to 0, so a seq-based cutoff derived from the
        // all-time high-water mark would evict every fresh low-seq segment each
        // cycle and re-upload + re-archive the on-disk window indefinitely
        // (duplicate recording_segments rows + wasted uploads — the root cause
        // the v0.1.68 playback dedup papered over). Recency-based eviction never
        // drops a name whose file is still on disk.
        let mut seen: HashSet<String> = HashSet::new();
        let mut seen_order: VecDeque<String> = VecDeque::new();
        // Highest segment seq this task has ever enqueued — the reference
        // point for detecting an FFmpeg counter reset (see the scan loop).
        let mut max_enqueued_seq: u64 = 0;
        // Supervisor restart counter — GROUND TRUTH for "the directory
        // was wiped and FFmpeg renumbered from 0".  The seq-regression
        // heuristic below stays as belt-and-suspenders, but it has a
        // dead band (previous run ≤ RESET_GAP segments) in which a
        // crash-looping camera's fresh files stayed `seen`-blocked
        // forever; the epoch covers every restart unconditionally.
        let mut last_restart_epoch: u64 =
            restart_epoch.load(std::sync::atomic::Ordering::SeqCst);
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
            // FFmpeg restarted since our last scan?  The supervisor
            // bumps the epoch on every SUCCESSFUL start, after cleanup()
            // wiped the directory — everything we remember refers to
            // deleted files.  (Failed starts don't bump: their cleanup
            // may have aborted partway, and no new files appear.)
            let epoch_now = restart_epoch.load(std::sync::atomic::Ordering::SeqCst);
            if epoch_now != last_restart_epoch {
                last_restart_epoch = epoch_now;
                if !seen.is_empty() {
                    tracing::info!(
                        "FFmpeg restart (epoch {}) for camera {} — clearing dedup state",
                        epoch_now, self.config.camera_id,
                    );
                }
                seen.clear();
                seen_order.clear();
                max_enqueued_seq = 0;
            }

            // Scan the output directory for new .ts segments.  Two passes:
            // collect EVERY parseable segment file first (including names
            // already in `seen`), because the FFmpeg-restart check below
            // needs to see the whole directory before the seen-filter
            // discards the evidence.
            let mut new_segments: Vec<(u64, PathBuf)> = Vec::new();
            let mut scan: Vec<(u64, String, PathBuf)> = Vec::new();

            match std::fs::read_dir(&self.config.output_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();
                        if !name.ends_with(".ts") {
                            continue;
                        }
                        if let Some(seq) = extract_sequence_number(&name) {
                            scan.push((seq, name, path));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read HLS directory: {}", e);
                }
            }

            // Completed-segment gate.  The muxer writes segment files IN
            // PLACE, flushing its AVIO buffer ~10x/second across each
            // segment's write window — so at any poll the newest on-disk
            // file is almost always still being written.  The old
            // "len >= 188" check only proved one TS packet existed; we
            // were routinely enqueueing the in-progress file and reading
            // a TRUNCATED prefix of it (upload + encrypted archive +
            // motion each captured a different partial state, and the
            // dedup set guaranteed the full version was never re-pushed).
            // The playlist is the muxer's own completion signal: a
            // segment is appended to stream.m3u8 only after its file is
            // closed (the same invariant the snapshot path relies on).
            // Only names listed there are eligible.  No playlist yet →
            // nothing is provably complete → wait a cycle (~1s).
            let completed: Option<std::collections::HashSet<String>> =
                std::fs::read_to_string(self.config.output_dir.join("stream.m3u8"))
                    .ok()
                    .map(|pl| {
                        pl.lines()
                            .map(str::trim)
                            .filter(|l| !l.starts_with('#') && l.ends_with(".ts"))
                            .map(|l| {
                                l.rsplit(['/', '\\']).next().unwrap_or(l).to_string()
                            })
                            .collect()
                    });

            // FFmpeg counter-reset detection.  On restart the supervisor's
            // generator cleanup() wipes the directory and FFmpeg numbers
            // from segment_00000.ts again.  If the previous run was short
            // (< SEEN_CAP segments), those low-seq NAMES are still in
            // `seen`, so without this check the fresh files would be
            // silently skipped — no upload, no motion detection, no
            // archive — until the new counter overtook the old one (a
            // crash-looping camera could stay dark indefinitely).  A
            // whole-directory max far below our enqueued high-water mark
            // can only mean the counter restarted: clear the dedup state.
            const RESET_GAP: u64 = 50;
            if let Some(scan_max) = scan.iter().map(|(s, ..)| *s).max() {
                if max_enqueued_seq > RESET_GAP
                    && scan_max.saturating_add(RESET_GAP) < max_enqueued_seq
                {
                    tracing::info!(
                        "Segment counter reset detected for camera {} (disk max {} \
                         vs enqueued max {}) — clearing dedup state after FFmpeg restart",
                        self.config.camera_id, scan_max, max_enqueued_seq,
                    );
                    seen.clear();
                    seen_order.clear();
                    max_enqueued_seq = 0;
                }
            }

            // Backstop floor for the playlist gate: any on-disk seq
            // STRICTLY OLDER than the oldest name still listed has
            // rotated out of the playlist window (hls_list_size=15) —
            // i.e. the muxer closed it long ago.  Without this, a
            // runtime stall >15s (e.g. a long retention pass) let
            // names rotate out before a scan ever saw them listed, and
            // the gate then blocked that backlog FOREVER (never
            // uploaded, never archived, swept from disk ~65s later).
            let completed_min_seq: Option<u64> = completed.as_ref().and_then(|done| {
                done.iter().filter_map(|n| extract_sequence_number(n)).min()
            });

            for (seq, name, path) in scan {
                if seen.contains(&name) {
                    continue;
                }
                // Playlist gate — only muxer-completed segments (above),
                // plus the rotated-out backlog floor.
                let listed = matches!(completed, Some(ref done) if done.contains(&name));
                let rotated_out = matches!(completed_min_seq, Some(min) if seq < min);
                if !listed && !rotated_out {
                    continue;
                }
                // Stat only actual candidates (not every file every
                // second): one final corruption guard — a listed file
                // smaller than one TS packet (188 bytes) is garbage.
                let meta = std::fs::metadata(&path).ok();
                let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                if len < 188 {
                    continue;
                }
                // The rotated-out path lacks the playlist's "muxer
                // closed it" guarantee — it's an inference from seq
                // ordering, and a stale playlist surviving a failed
                // cleanup() could make a BRAND-NEW in-progress file
                // (seq 0 after counter reset) look rotated-out.
                // Require write-quiescence: only enqueue via the
                // backstop once the file hasn't been modified for 3s
                // (segments write for ~1s; anything genuinely rotated
                // out has been closed for >=15s).  Unknown mtime →
                // wait, the next pass settles it.
                if !listed {
                    let quiescent = meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .map(|age| age >= std::time::Duration::from_secs(3))
                        .unwrap_or(false);
                    if !quiescent {
                        continue;
                    }
                }
                seen.insert(name.clone());
                seen_order.push_back(name);
                new_segments.push((seq, path));
            }

            // Process new segments in sequence order
            new_segments.sort_by_key(|(seq, _)| *seq);
            if let Some((max_seq, _)) = new_segments.last() {
                max_enqueued_seq = max_enqueued_seq.max(*max_seq);
            }

            // Prune the seen set to prevent unbounded growth, evicting the
            // oldest-discovered names first. The cap (200) is far larger than
            // the on-disk segment count (FFmpeg's hls_list_size ~15 plus the
            // orphan-sweep tail ~60), so an evicted name's file is always long
            // gone from disk and can never reappear to be re-processed.
            const SEEN_CAP: usize = 200;
            while seen_order.len() > SEEN_CAP {
                if let Some(old) = seen_order.pop_front() {
                    seen.remove(&old);
                }
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
                let codec_detected = self.codec_detected.clone();
                let api_client = self.api_client.clone();
                let cam_id_for_playlist = self.config.camera_id.clone();
                let output_dir = self.config.output_dir.clone();
                let dash = dash.clone();
                let camera_name = camera_name.to_string();
                let rec_state = self.recording_state.clone();
                let db = self.db.clone();
                let motion_cfg = self.motion_config.clone();
                let last_motion = last_motion.clone();

                // Spawn upload as a concurrent task so it doesn't block
                // the next segment. This prevents one slow upload from
                // stalling the entire pipeline. The per-camera semaphore
                // limits concurrent uploads to avoid unbounded task growth.
                let sem = self.upload_semaphore.clone();
                spawn_logged("segment-upload", async move {
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

                            // Codec detection (only first successful segment).
                            //
                            // Claim the slot atomically before doing any work,
                            // not after.  Four concurrent upload tasks
                            // (UPLOAD_SEMAPHORE = 4) used to all see
                            // `codec_detected == false` and race through
                            // detect_codec + set_codec + report_codec,
                            // hitting the backend with 4 identical codec
                            // reports per camera-start.  compare_exchange
                            // makes the first task to arrive the only one
                            // that runs the detection.
                            //
                            // Trade-off: if `report_codec` fails after we
                            // claimed the slot, no other task in this run
                            // will retry — the next process boot re-runs
                            // detection on the first segment then.  That's
                            // acceptable: the codec is also rendered into
                            // the dashboard regardless of CC's view, and
                            // CC is the only consumer of the report.
                            if codec_detected
                                .compare_exchange(
                                    false,
                                    true,
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                )
                                .is_ok()
                            {
                                if let Ok(info) = super::codec_detector::detect_codec(&segment_path) {
                                    dash.set_codec(&camera_name, &info.video_codec, &info.audio_codec);
                                    if api_client
                                        .report_codec(&camera_id, &info.video_codec, &info.audio_codec)
                                        .await
                                        .is_ok()
                                    {
                                        dash.log_info("Codec reported to cloud");
                                    }
                                }
                            }

                            // Motion detection (non-blocking, with per-camera cooldown).
                            //
                            // Skipped entirely in Local mode: detection costs a
                            // full FFmpeg decode of every segment (~30-50% of a
                            // Pi core per camera, forever), and Local mode has
                            // no consumer — report_motion short-circuits Ok(())
                            // and the local web UI has no motion surface.  All
                            // of that CPU bought a debug log line.
                            if motion_cfg.enabled && !api_client.is_local() {
                                spawn_motion_detection(
                                    segment_path.clone(),
                                    camera_id.clone(),
                                    seq,
                                    &motion_cfg,
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
                            spawn_logged("playlist-push", async move {
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
                                // Surface both file-read failures and DB
                                // write failures as warn-level log lines.
                                // Pre-v0.1.59 both were `let _ = …` silent
                                // discards, which meant the operator could
                                // toggle Record in CC and see segments
                                // never reach the archive without any
                                // diagnostic.  Failures here are real bugs
                                // (FS permission, disk full, DB corrupt)
                                // — the operator deserves to see them.
                                match tokio::fs::read(&segment_path).await {
                                    Ok(data) => {
                                        // Parse #EXTINF for this segment
                                        // from stream.m3u8 so the dynamic
                                        // playback playlist (Phase B local
                                        // web UI) has accurate per-segment
                                        // durations.  FFmpeg's HLS muxer
                                        // with target=1s mostly produces
                                        // ~1.000s segments but boundaries
                                        // vary; fall back to 1000ms when
                                        // the playlist isn't readable yet
                                        // (e.g. supervisor just restarted).
                                        let duration_ms = read_segment_duration_ms(
                                            &output_dir,
                                            seq,
                                        )
                                        .await
                                        .unwrap_or(1000);
                                        // spawn_blocking: this encrypts the
                                        // segment (software AES on Pi) and
                                        // takes the shared DB mutex — which
                                        // retention holds through multi-
                                        // second passes.  Run inline on a
                                        // tokio worker, up to 4 archive
                                        // tasks parked here starved the
                                        // whole 4-worker Pi runtime (scan
                                        // loops, HTTP, WS) for the duration
                                        // — the stall that then chopped
                                        // playlist-gate upload gaps.
                                        let blocking_db = db.clone();
                                        let arc_cam = camera_id.clone();
                                        let arc_today = today.clone();
                                        let write_result = tokio::task::spawn_blocking(
                                            move || {
                                                blocking_db.save_recording_segment(
                                                    &arc_cam, seq, &arc_today,
                                                    &data, duration_ms,
                                                )
                                            },
                                        )
                                        .await;
                                        match write_result {
                                            Ok(Ok(())) => {}
                                            Ok(Err(e)) => {
                                                dash.log_warn(format!(
                                                    "Recording archive: DB write failed for segment {} ({}): {}",
                                                    seq, camera_id, e,
                                                ));
                                            }
                                            Err(e) => {
                                                dash.log_warn(format!(
                                                    "Recording archive: write task panicked for segment {} ({}): {}",
                                                    seq, camera_id, e,
                                                ));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        dash.log_warn(format!(
                                            "Recording archive: can't read segment {} from disk: {}",
                                            seq, e,
                                        ));
                                    }
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

/// Spawn a background task that runs FFmpeg scene-change detection on
/// a segment and, if motion exceeds the threshold and the per-camera
/// cooldown has elapsed, POSTs a `motion_detected` event to the
/// backend.  Delivery is HTTP-only via `api_client.report_motion`;
/// pre-v0.1.61 a `motion_tx` `mpsc::Sender` was also threaded through
/// here for a never-implemented WS-forwarding path, removed in v0.1.61.
fn spawn_motion_detection(
    segment_path: PathBuf,
    camera_id: String,
    seq: u64,
    motion_cfg: &MotionConfig,
    last_motion: Arc<Mutex<Option<Instant>>>,
    api_client: ApiClient,
) {
    let threshold = motion_cfg.threshold;
    let cooldown = std::time::Duration::from_secs(motion_cfg.cooldown_secs);

    spawn_logged("motion-detect", async move {
        // Cooldown PEEK before the decode — not a claim.  detect_motion
        // spawns a full FFmpeg decode of the segment; running it first
        // and checking the cooldown after meant every cooldown window
        // burned ~cooldown_secs worth of pointless decodes per camera
        // (~29 wasted FFmpeg runs per 30s window on a Pi).  The claim
        // itself stays AFTER detection so a below-threshold segment
        // never arms the cooldown and suppresses real motion.
        {
            let guard = last_motion
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(last) = *guard {
                if last.elapsed() < cooldown {
                    return;
                }
            }
        }

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

    // ── Per-camera upload semaphore ───────────────────────────────────
    //
    // Locks in the fix for the cross-camera starvation bug: the upload
    // limiter used to be a single process-global Semaphore(4), so one
    // camera on a slow uplink could hold all permits and stall every
    // other camera.  Each uploader instance must now own its own.

    #[test]
    fn each_uploader_gets_its_own_upload_semaphore() {
        // Build two real HlsUploaders (the production constructor) for
        // two cameras and assert their semaphores are DISTINCT Arc
        // allocations with the full per-camera permit budget.  This is
        // the actual regression guard: if someone reverts to a shared
        // `static` semaphore, Arc::ptr_eq flips to true and this fails.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db");
        let rec_state = Arc::new(RwLock::new(HashSet::new()));

        let make = |cam: &str| {
            HlsUploader::new(
                HlsUploaderConfig::new(cam.into(), tmp.path().join(cam)),
                ApiClient::local_stub().expect("stub"),
                rec_state.clone(),
                db.clone(),
                MotionConfig::default(),
            )
        };
        let a = make("cam_a");
        let b = make("cam_b");

        assert!(
            !Arc::ptr_eq(&a.upload_semaphore, &b.upload_semaphore),
            "per-camera semaphores must be distinct allocations — a shared \
             static would let one slow camera starve the others",
        );
        assert_eq!(
            a.upload_semaphore.available_permits(),
            MAX_CONCURRENT_UPLOADS_PER_CAMERA,
        );
    }

    // ── spawn_logged panic guard ──────────────────────────────────────

    #[tokio::test]
    async fn spawn_logged_runs_a_normal_future_to_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        spawn_logged("test-ok", async move {
            r.store(true, Ordering::SeqCst);
        });
        // Yield + brief sleep so the detached task gets scheduled.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(ran.load(Ordering::SeqCst), "normal future should run");
    }

    #[tokio::test]
    async fn spawn_logged_contains_a_panic_and_keeps_runtime_alive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // A panicking detached task must not poison the runtime.  We
        // can't easily capture the tracing::error! line in a unit test,
        // but we CAN prove the runtime survived: a task spawned AFTER
        // the panicking one still runs to completion.
        spawn_logged("test-panic", async move {
            panic!("intentional test panic — should be caught + logged");
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let ran_after = Arc::new(AtomicBool::new(false));
        let r = ran_after.clone();
        spawn_logged("test-after-panic", async move {
            r.store(true, Ordering::SeqCst);
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            ran_after.load(Ordering::SeqCst),
            "runtime must survive a caught panic and keep scheduling tasks",
        );
    }
}
