// Sentinel CameraNode - Camera streaming node for Sentinel Command Center
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
//! Shared command implementations used by both the WebSocket inbound
//! command dispatcher (Connected mode) and the local `/api/*` HTTP
//! routes (Local + Connected modes — Phase B web UI).
//!
//! Why a shared module: pre-Phase-B, `cmd_take_snapshot` lived inside
//! `api::websocket` and was reachable only via a CC-issued WS message.
//! The local web UI needs the same capture flow over HTTP; rather than
//! duplicate the FFmpeg-frame-grab + DB-save code we lift it here and
//! both layers call it.

use std::path::{Path, PathBuf};

use crate::storage::NodeDatabase;

/// Metadata returned by [`take_snapshot`] — what both the WS dispatcher
/// and the HTTP route need.  WS transport additionally base64-encodes
/// the JPEG bytes (see [`fetch_snapshot_jpeg`]); HTTP returns a URL
/// pointing at `/api/snapshots/{id}` so the SPA can lazy-load the
/// image with the browser's normal image cache.
#[derive(Debug, serde::Serialize)]
pub struct SnapshotMeta {
    pub id: i64,
    pub camera_id: String,
    pub filename: String,
    /// Unix milliseconds — same as `SnapshotRecord::timestamp`.
    pub timestamp: i64,
    pub size_bytes: u64,
    /// When the frame was recorded (Unix ms): the start of its segment.
    /// `None` when the segment's neighbours could not be read.
    pub frame_time_ms: Option<i64>,
}

/// Capture a snapshot from the latest *complete* HLS segment for the
/// given camera, persist it to the DB (encrypted), and return its
/// metadata.
///
/// Returns the inserted row's id so callers can fetch the JPEG bytes
/// later via `db.get_snapshot_data(id)` — keeps the function lean
/// (no base64-encoding the bytes here; the WS path does that step
/// after this function returns).
///
/// Errors as a `String` so the WS dispatcher's existing error envelope
/// keeps working without a new error-type plumbing.
///
/// With `at_ms` (Unix ms), the frame comes from the segment recorded at
/// that moment instead of the newest one, so Sentinel AI can look at what
/// set off a motion alert after it is over. An `at_ms` older than every
/// segment still on disk is an error rather than a different moment.
pub async fn take_snapshot(
    camera_id: &str,
    hls_base_dir: &Path,
    db: &NodeDatabase,
    at_ms: Option<i64>,
) -> std::result::Result<SnapshotMeta, String> {
    let camera_hls_dir = hls_base_dir.join(camera_id);
    let timed = timed_segments(&camera_hls_dir);
    let latest_segment = match at_ms {
        Some(at) => segment_at(&timed, at)?,
        None => find_latest_segment(&camera_hls_dir),
    }
    .ok_or_else(|| format!("No segments found for camera {}", camera_id))?;
    let frame_time_ms = timed
        .iter()
        .find(|s| s.path == latest_segment)
        .map(|s| s.start_ms);

    // FFmpeg writes the one JPEG frame to its stdout, and it goes
    // straight into the encrypted database. It used to go through a
    // temp file, which left a plain image in the shared temp folder —
    // readable by other local accounts — until it was deleted (found
    // in the 2026-10-19 security audit). A pipe also cannot collide
    // between two snapshots taken at once, which the temp file's name
    // had to work hard to avoid.
    let ffmpeg = crate::streaming::find_ffmpeg();

    let output = tokio::process::Command::new(&ffmpeg)
        .args([
            "-y",
            "-i",
            &latest_segment.to_string_lossy(),
            "-frames:v",
            "1",
            "-q:v",
            "2",
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("FFmpeg failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last_line = stderr.lines().last().unwrap_or("unknown error");
        return Err(format!("FFmpeg error: {}", last_line));
    }

    let data = output.stdout;
    if data.is_empty() {
        return Err("FFmpeg produced no frame".into());
    }

    let now = chrono::Utc::now();
    let filename = format!(
        "{}_{}.jpg",
        camera_id.replace(['/', '\\'], "_"),
        now.format("%Y%m%d_%H%M%S"),
    );
    let timestamp = now.timestamp_millis();
    let size = data.len() as u64;

    // Safety floor: if the host disk is critically low, skip the
    // durable DB write but still return metadata so the operator gets
    // a usable response — they wanted a snapshot, not getting one is
    // a worse failure than not archiving it.  Caller handles the
    // "we returned an id but the row doesn't exist" edge by checking
    // SnapshotMeta.id == 0.
    if crate::storage::should_pause_recording() {
        tracing::warn!(
            "snapshot skipped DB write: host disk under safety floor (camera {})",
            camera_id,
        );
        return Ok(SnapshotMeta {
            id: 0,
            camera_id: camera_id.to_string(),
            filename,
            timestamp,
            size_bytes: size,
            frame_time_ms,
        });
    }

    // `save_snapshot` returns the inserted rowid directly under its
    // own connection lock — race-free even when two snapshots fire on
    // the same camera in the same millisecond.  The previous separate
    // `last_snapshot_id_for` lookup had a real race window where both
    // callers got the later row's id.
    let id = db
        .save_snapshot(camera_id, &filename, timestamp, &data)
        .map_err(|e| format!("DB save error: {}", e))?;

    tracing::info!("Snapshot captured: {} ({} bytes)", filename, size);

    Ok(SnapshotMeta {
        id,
        camera_id: camera_id.to_string(),
        filename,
        timestamp,
        size_bytes: size,
        frame_time_ms,
    })
}

/// A complete segment with the moment its first frame was recorded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimedSegment {
    pub path: PathBuf,
    /// When the segment started: its predecessor's finish time, or its
    /// own finish time less one second (the segment length) for the
    /// oldest one on disk.
    pub start_ms: i64,
    /// When FFmpeg finished writing it (the file's modification time).
    pub end_ms: i64,
}

/// The complete segments on disk, oldest first, with their times. The
/// newest file is left out: FFmpeg may still be writing it.
pub(crate) fn timed_segments(dir: &Path) -> Vec<TimedSegment> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut segs: Vec<(u64, PathBuf, i64)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let seq = name
                .strip_suffix(".ts")?
                .rsplit_once('_')?
                .1
                .parse::<u64>()
                .ok()?;
            let modified = e.metadata().ok()?.modified().ok()?;
            let end_ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as i64;
            Some((seq, e.path(), end_ms))
        })
        .collect();
    segs.sort_unstable_by_key(|(seq, _, _)| *seq);
    segs.pop(); // possibly still being written
    let mut previous_end: Option<i64> = None;
    segs.into_iter()
        .map(|(_, path, end_ms)| {
            let start_ms = previous_end.unwrap_or(end_ms - 1000);
            previous_end = Some(end_ms);
            TimedSegment {
                path,
                start_ms,
                end_ms,
            }
        })
        .collect()
}

/// The segment recorded at `at_ms`: the first that finished after it. A
/// moment after the newest complete segment gets the newest; one before
/// the oldest is refused, because "a frame from another time" would be
/// read as "this is what happened then".
pub(crate) fn segment_at(
    segments: &[TimedSegment],
    at_ms: i64,
) -> std::result::Result<Option<PathBuf>, String> {
    let Some(oldest) = segments.first() else {
        return Ok(None);
    };
    if at_ms < oldest.start_ms {
        return Err(format!(
            "no video retained from that moment: the oldest this node still has \
             starts at {} (Unix ms), {} s later",
            oldest.start_ms,
            (oldest.start_ms - at_ms) / 1000
        ));
    }
    Ok(segments
        .iter()
        .find(|s| s.end_ms > at_ms)
        .or_else(|| segments.last())
        .map(|s| s.path.clone()))
}

/// Convenience wrapper for the WS transport, which historically returned
/// the JPEG bytes inline as base64.  Reads the saved snapshot back from
/// the DB so the encrypt/decrypt round-trip is exercised on every WS
/// snapshot call (matches pre-refactor behaviour exactly).
pub async fn fetch_snapshot_jpeg(
    db: &NodeDatabase,
    id: i64,
) -> std::result::Result<Vec<u8>, String> {
    if id <= 0 {
        return Err("snapshot id missing — DB write was skipped (disk safety floor)".into());
    }
    db.get_snapshot_data(id).map_err(|e| e.to_string())
}

/// Find the newest *complete* `.ts` segment for a camera.
///
/// **Primary**: parse `stream.m3u8` — segments listed there are
/// guaranteed complete (the one currently being written has not been
/// appended to the playlist yet).
///
/// **Fallback**: filesystem scan using the *second*-to-latest sequence
/// number, since the very latest on disk may still be under active
/// write by FFmpeg.
///
/// Defence-in-depth: any returned path is verified to live inside
/// `dir` after canonicalisation.  Callers that pass an attacker-
/// controlled `dir` (the snapshot HTTP route's `hls_base_dir.join(
/// camera_id)` is the only one in v0.1.52) get a hard guarantee
/// that the FFmpeg `-i` argument can't traverse out, even if the
/// route-level allowlist is bypassed in the future.
pub(crate) fn find_latest_segment(dir: &Path) -> Option<PathBuf> {
    let dir_real = std::fs::canonicalize(dir).ok()?;
    // Try the playlist first, fall back to the FS scan if the playlist
    // produced no candidate OR the candidate doesn't live under `dir`.
    // A momentarily-malformed playlist entry (HLS muxer atomic rename
    // mid-read is rare but possible on Windows) shouldn't cause the
    // whole snapshot to fail when valid in-tree segments still exist.
    let try_candidate = |seg: PathBuf| -> Option<PathBuf> {
        let seg_real = std::fs::canonicalize(&seg).ok()?;
        if seg_real.starts_with(&dir_real) {
            Some(seg)
        } else {
            tracing::warn!(
                "find_latest_segment refused out-of-tree path: dir={} seg={}",
                dir_real.display(),
                seg_real.display(),
            );
            None
        }
    };
    if let Some(seg) = last_segment_from_playlist(dir).and_then(try_candidate) {
        return Some(seg);
    }
    last_segment_from_fs(dir).and_then(try_candidate)
}

/// Parse `stream.m3u8` and return the path of the last `.ts` entry.
fn last_segment_from_playlist(dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(dir.join("stream.m3u8")).ok()?;
    let seg_line = content
        .lines()
        .rev()
        .find(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && t.ends_with(".ts")
        })?
        .trim();
    // The entry may carry a relative/absolute prefix — normalise to a
    // plain filename so we can resolve it against `dir`.
    let filename = std::path::Path::new(seg_line).file_name()?;
    let path = dir.join(filename);
    path.is_file().then_some(path)
}

/// Scan the directory for `segment_<seq>.ts` files and return the
/// *second*-to-latest by sequence number — the latest may be incomplete.
fn last_segment_from_fs(dir: &Path) -> Option<PathBuf> {
    let mut segs: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".ts") {
                return None;
            }
            let parts: Vec<&str> = name.split('_').collect();
            if parts.len() != 2 {
                return None;
            }
            let seq = parts[1].trim_end_matches(".ts").parse::<u64>().ok()?;
            Some((seq, e.path()))
        })
        .collect();
    if segs.is_empty() {
        return None;
    }
    segs.sort_unstable_by_key(|(seq, _)| *seq);
    if segs.len() >= 2 {
        segs.pop();
    }
    segs.pop().map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    //! Segment-selection tests, moved here from `api::websocket` in
    //! Phase B together with the helpers they exercise.  Pin the
    //! "playlist over FS / second-to-latest fallback" rules that
    //! prevent the snapshot grab from racing FFmpeg's in-progress
    //! segment write.
    use super::*;

    #[test]
    fn segment_selection_prefers_playlist_over_fs() {
        let dir = std::env::temp_dir().join("sbs_cmds_test_playlist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("segment_00001.ts"), b"seg1").unwrap();
        std::fs::write(dir.join("segment_00002.ts"), b"seg2").unwrap();
        std::fs::write(dir.join("segment_00003.ts"), b"incomplete").unwrap();
        std::fs::write(
            dir.join("stream.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\nsegment_00001.ts\n#EXTINF:1.0,\nsegment_00002.ts\n",
        )
        .unwrap();

        let result = find_latest_segment(&dir);
        assert_eq!(result.unwrap().file_name().unwrap(), "segment_00002.ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_selection_fs_fallback_skips_latest() {
        let dir = std::env::temp_dir().join("sbs_cmds_test_fs_fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("segment_00001.ts"), b"seg1").unwrap();
        std::fs::write(dir.join("segment_00002.ts"), b"seg2").unwrap();
        std::fs::write(dir.join("segment_00003.ts"), b"seg3").unwrap();

        let result = find_latest_segment(&dir);
        assert_eq!(result.unwrap().file_name().unwrap(), "segment_00002.ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_selection_single_segment_still_returned() {
        let dir = std::env::temp_dir().join("sbs_cmds_test_single_seg");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("segment_00001.ts"), b"seg1").unwrap();

        let result = find_latest_segment(&dir);
        assert_eq!(result.unwrap().file_name().unwrap(), "segment_00001.ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_parser_handles_path_prefix() {
        let dir = std::env::temp_dir().join("sbs_cmds_test_path_prefix");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("segment_00005.ts"), b"seg5").unwrap();
        std::fs::write(
            dir.join("stream.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\n./data/hls/cam1/segment_00005.ts\n",
        )
        .unwrap();

        let result = last_segment_from_playlist(&dir);
        assert_eq!(result.unwrap().file_name().unwrap(), "segment_00005.ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If the playlist names a `.ts` file outside `dir`,
    /// `find_latest_segment` must reject it after canonicalisation.
    /// Regression guard for the path-traversal class fixed in v0.1.52.
    #[test]
    fn find_latest_segment_rejects_out_of_tree_target() {
        let parent = std::env::temp_dir().join("sbs_cmds_test_traversal_parent");
        let dir = parent.join("inner");
        let outside = parent.join("outside");
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Place a "real" segment outside the camera dir.
        let outside_seg = outside.join("segment_00001.ts");
        std::fs::write(&outside_seg, b"hostile").unwrap();

        // Write a playlist inside the camera dir that points at the
        // outside segment via `..`.  Without the canonicalisation
        // guard, find_latest_segment would happily return it.
        std::fs::write(
            dir.join("stream.m3u8"),
            "#EXTM3U\n#EXTINF:1.0,\n../outside/segment_00001.ts\n",
        )
        .unwrap();
        // Don't put any in-tree segment — we want to confirm the
        // function returns None rather than the outside file.

        let result = find_latest_segment(&dir);
        assert!(
            result.is_none(),
            "find_latest_segment must refuse paths outside dir, got {:?}",
            result,
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Five one-second segments finishing at 10s..14s (Unix seconds), the
    /// last still "being written".
    fn timed_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, end) in (10..15).enumerate() {
            let path = dir.join(format!("segment_{:05}.ts", i));
            let file = std::fs::File::create(&path).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(end))
                .unwrap();
        }
        dir
    }

    #[test]
    fn timed_segments_skip_the_one_being_written_and_chain_start_times() {
        let dir = timed_dir("sbs_cmds_timed_chain");
        let segs = timed_segments(&dir);
        assert_eq!(segs.len(), 4, "segment_00004 may still be open");
        assert_eq!((segs[0].start_ms, segs[0].end_ms), (9_000, 10_000));
        assert_eq!((segs[2].start_ms, segs[2].end_ms), (11_000, 12_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_moment_picks_the_segment_recorded_then() {
        let dir = timed_dir("sbs_cmds_timed_pick");
        let segs = timed_segments(&dir);
        let pick = |at| segment_at(&segs, at).unwrap().unwrap();
        assert!(pick(11_500).ends_with("segment_00002.ts"), "11.5s is in 11-12s");
        assert!(pick(9_000).ends_with("segment_00000.ts"), "the oldest frame");
        // Newer than anything complete: the newest complete segment.
        assert!(pick(60_000).ends_with("segment_00003.ts"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A moment that has already been deleted must not quietly become
    /// "the oldest frame we have": that would be read as what happened.
    #[test]
    fn a_moment_older_than_the_retained_video_is_refused() {
        let dir = timed_dir("sbs_cmds_timed_old");
        let segs = timed_segments(&dir);
        let err = segment_at(&segs, 1_000).unwrap_err();
        assert!(err.contains("no video retained"), "{err}");
        assert_eq!(segment_at(&[], 1_000).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Snapshots never touch the shared temp folder: FFmpeg pipes the
    /// JPEG straight back (2026-10-19 security audit). Needs FFmpeg, and
    /// is skipped where there is none.
    #[tokio::test]
    async fn a_snapshot_is_piped_from_ffmpeg_and_never_written_to_temp() {
        let ffmpeg = crate::streaming::find_ffmpeg();
        if std::process::Command::new(&ffmpeg).arg("-version").output().is_err() {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let cam = base.path().join("cam1");
        std::fs::create_dir_all(&cam).unwrap();
        let status = std::process::Command::new(&ffmpeg)
            .args([
                "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10",
                "-t", "3", "-c:v", "mpeg2video", "-f", "hls", "-hls_time", "1",
                "-hls_list_size", "0", "-hls_segment_filename",
            ])
            .arg(cam.join("segment_%05d.ts"))
            .arg(cam.join("stream.m3u8"))
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg could not make test segments");

        let leftovers = || {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("sourcebox_sentry_snap_"))
                .count()
        };
        let before = leftovers();
        let db = crate::storage::NodeDatabase::new(&base.path().join("node.db")).unwrap();
        let meta = take_snapshot("cam1", base.path(), &db, None).await.unwrap();
        let jpeg = fetch_snapshot_jpeg(&db, meta.id).await.unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "a JPEG came back");
        assert_eq!(leftovers(), before, "nothing written to the temp folder");
    }
}
