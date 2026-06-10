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

//! Lightweight motion detection using FFmpeg scene-change scoring.
//!
//! Reads each completed HLS segment into memory and pipes it into an FFmpeg
//! child through stdin. FFmpeg runs `-vf "select='gte(scene,T)',metadata=print"`
//! against the piped MPEG-TS; if any frame exceeds `threshold` the segment is
//! flagged as containing motion and the peak score is returned.
//!
//! ### Why we pipe instead of passing a path
//!
//! On Windows, `DeleteFile` fails with `ERROR_SHARING_VIOLATION` unless every
//! open handle was created with `FILE_SHARE_DELETE`. Rust's `std::fs::File`
//! sets that flag; FFmpeg's own file I/O does not. Earlier versions passed
//! `-i <path>` which made FFmpeg open the segment directly — that handle then
//! raced the HLS muxer's rotation, producing `failed to delete old segment`
//! warnings and leaving the rotated `.ts` on disk until the uploader's
//! sweeper reaped it. Reading the bytes in Rust first closes our handle
//! before FFmpeg ever sees the content, so rotation is unblocked.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// Maximum time FFmpeg is allowed to run per segment before we give up.
/// With the 320x180 downscale, even a 2-second segment should analyse in
/// well under 5 seconds.  If it takes longer, something is broken.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(5);

/// Analyse a `.ts` segment for motion using FFmpeg scene-change detection.
///
/// Returns `Some(peak_score)` if any frame's scene score exceeds `threshold`,
/// or `None` if there is no significant motion (or FFmpeg fails/times out).
pub async fn detect_motion(segment_path: &Path, threshold: f64) -> Option<f64> {
    // Read the segment bytes via Rust so the file handle carries
    // FILE_SHARE_DELETE on Windows (see module docs). The bytes land in a
    // Vec<u8> then the handle closes, clearing any delete-block before we
    // even spawn FFmpeg. Typical segment is 200–400 KB, so this adds
    // negligible memory pressure.
    let bytes = match tokio::fs::read(segment_path).await {
        Ok(b) => b,
        Err(_) => return None, // segment rotated away, or I/O failure
    };

    let ffmpeg = super::find_ffmpeg();

    // Always extract ALL scene scores (threshold 0 in FFmpeg), then compare
    // against the configured threshold in Rust.  This lets us log actual peak
    // scores for debugging even when they fall below the trigger threshold.
    let work = async {
        let mut child = tokio::process::Command::new(&ffmpeg)
            .args([
                "-f", "mpegts",
                "-i", "pipe:0",
                // fps BEFORE scale: drop to 5 fps first, THEN scale only
                // the 5 surviving frames — the old order scaled all ~30
                // decoded frames and threw 25 of them away.  (Decode
                // still dominates, but the reorder is free.)
                "-vf", "fps=5,scale=320:180,select='gte(scene,0)',metadata=print",
                "-an",
                "-f", "null",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .ok()?;

        // Feed the segment to FFmpeg, then drop our stdin writer so FFmpeg
        // sees EOF and finishes. Without the drop, wait_with_output() would
        // stall forever waiting for us to close stdin.
        {
            let mut stdin = child.stdin.take()?;
            stdin.write_all(&bytes).await.ok()?;
        }

        child.wait_with_output().await.ok()
    };

    let output = match tokio::time::timeout(FFMPEG_TIMEOUT, work).await {
        Ok(Some(o)) => o,
        Ok(None) => return None, // FFmpeg failed to spawn / stdin pipe broke
        Err(_) => {
            tracing::warn!("Motion detection timed out for {}", segment_path.display());
            return None;
        }
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    let peak = parse_peak_scene_score(&stderr);

    match peak {
        Some(score) if score >= threshold => {
            tracing::info!(
                "Motion detected: score={:.6} threshold={:.4} segment={}",
                score, threshold, segment_path.display()
            );
            Some(score)
        }
        Some(score) => {
            tracing::debug!(
                "No motion: peak={:.6} < threshold={:.4} segment={}",
                score, threshold, segment_path.display()
            );
            None
        }
        None => None,
    }
}

/// Parse FFmpeg metadata output for the highest `lavfi.scene_score` value.
///
/// FFmpeg `metadata=print` outputs lines like:
///   `[Parsed_metadata_3 @ 0x...] lavfi.scene_score=0.482361`
/// We search for the `lavfi.scene_score=` substring anywhere in each line.
fn parse_peak_scene_score(stderr: &str) -> Option<f64> {
    const NEEDLE: &str = "lavfi.scene_score=";
    let mut peak: Option<f64> = None;

    for line in stderr.lines() {
        if let Some(pos) = line.find(NEEDLE) {
            let val_str = &line[pos + NEEDLE.len()..];
            if let Ok(score) = val_str.trim().parse::<f64>() {
                peak = Some(peak.map_or(score, |p: f64| p.max(score)));
            }
        }
    }

    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_peak_scene_score_found() {
        // Real FFmpeg output has a [Parsed_metadata_N @ 0x...] prefix
        let stderr = "\
[Parsed_metadata_3 @ 0x1234] frame:0    pts:0       pts_time:0
[Parsed_metadata_3 @ 0x1234] lavfi.scene_score=0.123456
[Parsed_metadata_3 @ 0x1234] frame:1    pts:3000    pts_time:0.033
[Parsed_metadata_3 @ 0x1234] lavfi.scene_score=0.654321
[Parsed_metadata_3 @ 0x1234] frame:2    pts:6000    pts_time:0.067
[Parsed_metadata_3 @ 0x1234] lavfi.scene_score=0.234567";
        assert_eq!(parse_peak_scene_score(stderr), Some(0.654321));
    }

    #[test]
    fn test_parse_peak_scene_score_none() {
        let stderr = "some random ffmpeg output\nno scores here";
        assert_eq!(parse_peak_scene_score(stderr), None);
    }

    #[test]
    fn test_parse_peak_scene_score_empty() {
        assert_eq!(parse_peak_scene_score(""), None);
    }
}
