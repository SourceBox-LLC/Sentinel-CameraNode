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
pub async fn take_snapshot(
    camera_id: &str,
    hls_base_dir: &Path,
    db: &NodeDatabase,
) -> std::result::Result<SnapshotMeta, String> {
    let camera_hls_dir = hls_base_dir.join(camera_id);
    let latest_segment = find_latest_segment(&camera_hls_dir)
        .ok_or_else(|| format!("No segments found for camera {}", camera_id))?;

    // Use FFmpeg to extract a single frame as JPEG.  Tempfile path is
    // process-unique and cleaned up below — multiple parallel snapshots
    // on the same camera would collide (rare but possible if HTTP and
    // WS both fire), so the suffix layers three pieces of entropy:
    //
    //   - `process::id()` distinguishes any future multi-instance
    //     deployment (Docker rebuilds, side-by-side test harnesses).
    //   - `SystemTime::now().as_nanos()` gives per-second granularity
    //     on every platform.
    //   - A per-process `AtomicU64` counter is the actual collision
    //     defence: on Windows `SystemTime::now()` has ~15 ms resolution
    //     (the system tick), so two snapshots fired within 15 ms get
    //     identical nanos — only the counter pulls them apart.  Without
    //     it the second FFmpeg invocation truncates the first's output,
    //     the loser's `tokio::fs::remove_file` deletes the survivor,
    //     and both callers see a corrupt JPEG.
    static SNAP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = SNAP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = std::env::temp_dir().join(format!(
        "sourcebox_sentry_snap_{}_{}_{}_{}.jpg",
        camera_id,
        std::process::id(),
        nanos,
        counter,
    ));

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
        ])
        .arg(&temp_path)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("FFmpeg failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last_line = stderr.lines().last().unwrap_or("unknown error");
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(format!("FFmpeg error: {}", last_line));
    }

    // Read the JPEG data and save to database.
    let data = tokio::fs::read(&temp_path)
        .await
        .map_err(|e| format!("Failed to read snapshot: {}", e))?;
    let _ = tokio::fs::remove_file(&temp_path).await;

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
    })
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
}
