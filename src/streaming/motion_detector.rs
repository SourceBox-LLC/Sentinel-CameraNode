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

//! Lightweight motion detection: the fraction of the picture that changed.
//!
//! Reads each completed HLS segment into memory and pipes it into an FFmpeg
//! child through stdin. At 5 fps and 320x180 grey, FFmpeg differences each
//! frame against the one before, counts the pixels whose brightness moved
//! by more than [`PIXEL_DELTA`], and prints that fraction per frame. If any
//! frame's fraction reaches `threshold` the segment has motion, and the
//! peak fraction is returned.
//!
//! ### Why not FFmpeg's scene score
//!
//! It was the first detector, and it scores how different the whole
//! picture is. A person crossing a dark yard changes a few percent of it,
//! so on the virtual-camera rig a person walking through an infrared night
//! scene peaked at 0.0085 against the 0.02 trigger and never fired, while
//! the same yard empty scored 0.0088 from sensor noise alone: the measure
//! could not tell the two apart at any threshold. Counting changed pixels
//! can, because sensor noise moves many pixels a little and a moving
//! subject moves fewer pixels a lot. Measured on the rig's footage (share
//! of pixels changed by more than 25): empty scenes, day and infrared
//! night, 0.003 to 0.03%; a person at night 0.4 to 1.2%; a porch theft 1 to
//! 9%. The default threshold, 0.15%, sits five times above the noisiest
//! empty scene.
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
/// How much a pixel's brightness (0-255) must move between two frames to
/// count as changed. Sensor noise, including infrared grain at night,
/// stays below it; a moving subject does not.
pub const PIXEL_DELTA: u8 = 25;

/// The default share of pixels that must change for motion (0.15%).
pub const DEFAULT_THRESHOLD: f64 = 0.0015;

/// The FFmpeg filter: changed-pixel mask per frame pair, and its mean.
fn changed_pixels_filter() -> String {
    format!(
        "fps=5,scale=320:180,format=gray,tblend=all_mode=difference,\
         lutyuv=y='if(gt(val,{PIXEL_DELTA}),255,0)',signalstats,\
         metadata=print:key=lavfi.signalstats.YAVG"
    )
}

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

    // Every frame's changed fraction is printed and compared against the
    // threshold here, so the peak can be logged even when it falls short.
    let filter = changed_pixels_filter();
    let work = async {
        let mut child = tokio::process::Command::new(&ffmpeg)
            .args([
                "-f", "mpegts",
                "-i", "pipe:0",
                // fps BEFORE scale: drop to 5 fps first, THEN scale only
                // the 5 surviving frames — the old order scaled all ~30
                // decoded frames and threw 25 of them away.  (Decode
                // still dominates, but the reorder is free.)
                "-vf", &filter,
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
    let peak = parse_peak_changed_fraction(&stderr);

    match peak {
        Some(score) if score >= threshold => {
            tracing::info!(
                "Motion detected: {:.3}% of pixels changed (threshold {:.3}%) segment={}",
                score * 100.0, threshold * 100.0, segment_path.display()
            );
            Some(score)
        }
        Some(score) => {
            tracing::debug!(
                "No motion: peak {:.3}% < threshold {:.3}% segment={}",
                score * 100.0, threshold * 100.0, segment_path.display()
            );
            None
        }
        None => None,
    }
}

/// The highest changed-pixel fraction (0.0-1.0) in FFmpeg's output.
///
/// `metadata=print` writes lines like
///   `[Parsed_metadata_6 @ 0x...] lavfi.signalstats.YAVG=0.912`
/// where YAVG is the mean of the 0/255 changed-pixel mask, so YAVG / 255
/// is the fraction of pixels that changed.
fn parse_peak_changed_fraction(stderr: &str) -> Option<f64> {
    const NEEDLE: &str = "lavfi.signalstats.YAVG=";
    stderr
        .lines()
        .filter_map(|line| {
            let pos = line.find(NEEDLE)?;
            line[pos + NEEDLE.len()..].trim().parse::<f64>().ok()
        })
        .map(|yavg| (yavg / 255.0).clamp(0.0, 1.0))
        .reduce(f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peak_changed_fraction_is_read_from_signalstats() {
        // Real FFmpeg output has a [Parsed_metadata_N @ 0x...] prefix.
        let stderr = "\
[Parsed_metadata_6 @ 0x1234] frame:0    pts:0       pts_time:0
[Parsed_metadata_6 @ 0x1234] lavfi.signalstats.YAVG=0.0255
[Parsed_metadata_6 @ 0x1234] frame:1    pts:3000    pts_time:0.2
[Parsed_metadata_6 @ 0x1234] lavfi.signalstats.YAVG=2.55
[Parsed_metadata_6 @ 0x1234] lavfi.signalstats.YAVG=0.51";
        let peak = parse_peak_changed_fraction(stderr).unwrap();
        assert!((peak - 0.01).abs() < 1e-9, "2.55 / 255 is 1% of pixels, got {peak}");
    }

    #[test]
    fn no_values_means_no_reading() {
        assert_eq!(parse_peak_changed_fraction("some ffmpeg output\nno stats"), None);
        assert_eq!(parse_peak_changed_fraction(""), None);
    }

    /// The rig's measurements, as fractions: the default must sit above
    /// every empty scene and below a person at night.
    #[test]
    fn the_default_threshold_separates_the_measured_scenes() {
        let empty_scenes = [0.0003, 0.0002, 0.0001]; // porch, night yard, driveway
        let person_at_night = 0.004; // lowest per-second peak while crossing
        assert!(empty_scenes.iter().all(|&e| e * 5.0 <= DEFAULT_THRESHOLD));
        assert!(person_at_night > DEFAULT_THRESHOLD);
    }

    /// The filter runs on a real segment: a frame with a moving square
    /// against a still background reports roughly the square's share.
    #[tokio::test]
    async fn the_filter_measures_a_moving_subject() {
        let ffmpeg = crate::streaming::find_ffmpeg();
        let dir = tempfile::tempdir().unwrap();
        let segment = dir.path().join("moving.ts");
        // A 32x32 white square moving across a black 320x180 frame.
        let made = std::process::Command::new(&ffmpeg)
            .args([
                "-v", "error", "-f", "lavfi", "-i",
                "color=black:s=320x180:r=5:d=1[bg];color=white:s=32x32:r=5:d=1[sq];\
                 [bg][sq]overlay=x='t*100':y=70",
                "-c:v", "mpeg2video", "-f", "mpegts",
            ])
            .arg(&segment)
            .status();
        let Ok(status) = made else { return }; // no FFmpeg on this machine
        if !status.success() {
            return;
        }
        let peak = detect_motion(&segment, DEFAULT_THRESHOLD)
            .await
            .expect("a moving square is motion");
        assert!(peak > 0.01 && peak < 0.1, "about 2 x 32x32 of 320x180, got {peak}");
        let still = dir.path().join("still.ts");
        std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i", "color=gray:s=320x180:r=5:d=1",
                   "-c:v", "mpeg2video", "-f", "mpegts"])
            .arg(&still)
            .status()
            .unwrap();
        assert_eq!(detect_motion(&still, DEFAULT_THRESHOLD).await, None);
    }
}
