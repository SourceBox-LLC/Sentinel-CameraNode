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
//! Linux camera detection using v4l2 API
//!
//! Scans for USB cameras on Linux systems by checking /dev/video* devices.
//!
//! ## Why this is more than "list /dev/videoN"
//!
//! On modern Linux kernels (≥5.x), each UVC USB camera registers
//! **multiple** `/dev/videoN` nodes — typically one for video capture
//! and a sibling node for V4L2 metadata (`V4L2_CAP_META_CAPTURE`).
//! Some drivers register even more (e.g. a separate node for the
//! ISP/encoder pipeline).  The result is that a single physical
//! camera can appear as 2–4 device nodes.
//!
//! If we naively treat every `/dev/videoN` as a camera and hand it to
//! FFmpeg, the metadata-only nodes blow up at stream-start with
//! cryptic exit codes (231, 240) because there's no video stream
//! there to read — see the production incident on the Raspberry Pi
//! where 2 physical USB cameras showed up as 4 entries with half of
//! them stuck in `failed: FFmpeg`.
//!
//! Four filters fix it (see [`LinuxDetector::detect_cameras`]):
//!   1. **Capability check** — `VIDIOC_QUERYCAP` must report
//!      `V4L2_CAP_VIDEO_CAPTURE`.  Excludes metadata-only siblings.
//!   2. **M2M reject** — drop nodes that set `V4L2_CAP_VIDEO_M2M` or
//!      `V4L2_CAP_VIDEO_M2M_MPLANE`.  These are codecs/transformers
//!      that need a frame pushed in before they emit one — FFmpeg
//!      can't capture from them and dies with EINVAL.
//!   3. **Driver blacklist** — Raspberry Pi's `bcm2835-isp` and
//!      `bcm2835-codec` drivers register single-direction capture
//!      nodes (so the M2M check doesn't catch them) but they're
//!      ISP/codec processing pipelines, not real camera sources.
//!      They load by default on Pi OS even with no CSI camera
//!      attached, so we have to filter them by driver name.
//!   4. **bus_info dedup** — multiple capture-capable nodes from the
//!      same physical USB device collapse to one entry; the kernel
//!      orders these such that the lowest videoN is canonical.

use std::collections::HashSet;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::CameraDetector;
use crate::camera::types::CameraCapabilities;
use crate::camera::DetectedCamera;
use crate::error::Result;

// ── V4L2 ioctl primitives ──────────────────────────────────────────
//
// We deliberately don't pull in the `v4l` crate for one ioctl — the
// surface here is small and stable (the v4l2_capability struct hasn't
// changed since kernel 1.4).
//
// The VIDIOC_QUERYCAP ioctl number is architecture-independent: the
// `_IOR('V', 0, struct v4l2_capability)` macro encodes only the
// direction (READ=2), type ('V'=0x56), nr (0), and struct size (104),
// which evaluate the same on x86_64, aarch64, and armv7.

/// `V4L2_CAP_VIDEO_CAPTURE` from <linux/videodev2.h>.  Set when a
/// device node can produce a single-planar video capture stream.
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;

/// `V4L2_CAP_VIDEO_M2M_MPLANE` — multi-planar memory-to-memory.
/// Codec/transform devices set this; they're never real cameras.
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;

/// `V4L2_CAP_VIDEO_M2M` — single-planar memory-to-memory.  Same
/// reasoning as above.
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_8000;

/// Driver-name prefixes for V4L2 nodes that report
/// `V4L2_CAP_VIDEO_CAPTURE` but aren't actually cameras you can
/// stream from.  These show up on Raspberry Pi OS by default
/// regardless of whether a CSI camera is attached, and FFmpeg dies
/// with EINVAL ("Inappropriate ioctl for device") if you point it
/// at one.
///
/// Match is case-sensitive prefix on `cap.driver` (truncated to 16
/// bytes by the kernel).  We avoid the broader `bcm2835-` prefix so
/// we don't accidentally exclude the unicam CSI camera driver
/// (`bcm2835-unicam`) which IS a real camera source.
const NON_CAMERA_DRIVER_PREFIXES: &[&str] = &[
    "bcm2835-isp",   // Raspberry Pi ISP pipeline (capture/output/stats nodes)
    "bcm2835-codec", // Raspberry Pi hardware H.264 codec (encoder + decoder)
];

/// `_IOR('V', 0, struct v4l2_capability)` — query device capabilities.
/// Same value across all Linux architectures we ship to (x86_64,
/// aarch64, armv7).
///
/// Stored as `u64` because the type `libc::ioctl` expects for its
/// `request` argument (aliased as `libc::Ioctl`) differs between libc
/// implementations:
///   * glibc / bionic / uclibc → `c_ulong` (u64 on 64-bit, u32 on 32-bit)
///   * musl                    → `c_int`   (i32 everywhere)
///
/// The cast to `libc::Ioctl` happens at the call site.  The kernel only
/// cares about the 32-bit ioctl number's bit pattern, not whether the
/// host type is signed or unsigned — `0x80685600 as i32` is negative but
/// its bits are identical to `0x80685600 as u32`, which is what the
/// syscall boundary actually compares against.  Without this the Alpine
/// Docker build (our only musl target) fails to compile with
/// "expected `i32`, found `u64`".
const VIDIOC_QUERYCAP: u64 = 0x8068_5600;

/// Mirrors `struct v4l2_capability` from <linux/videodev2.h>.  Layout
/// is part of the kernel uABI and stable since v4l2 1.4.  104 bytes
/// on every architecture.
#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// Result of a `VIDIOC_QUERYCAP` probe — the per-node capability mask,
/// the driver name (used to blacklist Pi ISP/codec nodes), and the
/// USB bus path we use to dedupe multi-node cameras.
struct V4l2Caps {
    /// Effective per-node capabilities (device_caps if the driver
    /// reports it, otherwise the legacy device-wide `capabilities`).
    capabilities: u32,
    /// Kernel driver name (e.g. "uvcvideo", "bcm2835-isp", "unicam").
    /// Used to filter out Pi ISP/codec nodes that report video-capture
    /// capability but aren't streamable cameras.  Truncated to 16 bytes
    /// by the kernel uABI.
    driver: String,
    /// USB bus path (e.g. "usb-0000:01:00.0-1.2"), used as the dedup
    /// key.  Empty for non-USB drivers, in which case dedup is a no-op
    /// for that node.
    bus_info: String,
}

/// Trim a kernel-supplied fixed-size NUL-terminated byte buffer down
/// to the printable string before the first NUL.  v4l2 returns
/// `driver`, `card`, and `bus_info` in fixed buffers and the bytes
/// past the NUL are unspecified — without this trim they show up as
/// trailing `\0` glyphs in logs and (worse) break our prefix matching
/// when Rust treats the trailing nulls as part of the string.
fn trim_kernel_cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Open `device_path` and run `VIDIOC_QUERYCAP` on it.
///
/// We use `device_caps` (V4L2 1.4+) when the driver fills it because
/// it's the *per-node* capability set — exactly the discriminator we
/// need to skip metadata-only sibling nodes.  Older drivers leave
/// `device_caps == 0` and only fill the union-across-nodes
/// `capabilities` field; we fall back to it then.
fn query_v4l2_caps(device_path: &str) -> Result<V4l2Caps> {
    let file = std::fs::File::open(device_path).map_err(|e| {
        crate::error::Error::Camera(format!("Cannot open {}: {}", device_path, e))
    })?;

    // SAFETY: `cap` is plain old data and the kernel will fully
    // populate it on success.  Zero-init is correct for V4L2 — the
    // ioctl writes the whole struct, never reads.
    let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            // Cast to the libc-implementation-specific `Ioctl` type.
            // Bit pattern is what matters to the kernel; the sign of
            // the host-side value is irrelevant.  See the constant's
            // definition for why this can't just be a typed constant.
            VIDIOC_QUERYCAP as libc::Ioctl,
            &mut cap as *mut V4l2Capability as *mut libc::c_void,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(crate::error::Error::Camera(format!(
            "VIDIOC_QUERYCAP({}) failed: {}",
            device_path, err
        )));
    }

    let effective = if cap.device_caps != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };

    Ok(V4l2Caps {
        capabilities: effective,
        driver: trim_kernel_cstr(&cap.driver),
        bus_info: trim_kernel_cstr(&cap.bus_info),
    })
}

pub struct LinuxDetector;

impl LinuxDetector {
    pub fn new() -> Self {
        Self
    }
}

impl CameraDetector for LinuxDetector {
    fn detect_cameras(&self) -> Result<Vec<DetectedCamera>> {
        let mut cameras = Vec::new();
        // Dedup key: kernel `bus_info` string from VIDIOC_QUERYCAP.
        // First node we see for a given physical camera wins; later
        // nodes belonging to the same camera (metadata sibling, ISP
        // pipeline, etc.) are skipped silently.
        let mut seen_bus_info: HashSet<String> = HashSet::new();

        tracing::info!("Scanning for USB cameras on Linux (v4l2)...");

        // Bumped from 0..10 to 0..32 because a system with two USB
        // cameras already reaches /dev/video3 once metadata siblings
        // are included; a hub plus a built-in webcam plus an HDMI
        // capture card will easily push past 10.  Existence check is
        // a single statx — the upper bound is essentially free.
        for i in 0..32u32 {
            let path = format!("/dev/video{}", i);

            if !Path::new(&path).exists() {
                continue;
            }

            // Filter 1: must support V4L2_CAP_VIDEO_CAPTURE.  This
            // alone excludes the metadata-only sibling nodes that
            // every UVC camera registers on modern kernels.
            let v4l_caps = match query_v4l2_caps(&path) {
                Ok(c) => c,
                Err(e) => {
                    // Open/ioctl failure — could be permissions
                    // (user not in `video` group), the device just
                    // got unplugged, or the driver doesn't support
                    // QUERYCAP.  Skip with a debug log; aborting the
                    // whole scan would lose the cameras we *can* read.
                    tracing::debug!("{}: skipping ({})", path, e);
                    continue;
                }
            };

            if v4l_caps.capabilities & V4L2_CAP_VIDEO_CAPTURE == 0 {
                tracing::debug!(
                    "{}: not a video capture device (caps=0x{:08x}), skipping",
                    path, v4l_caps.capabilities
                );
                continue;
            }

            // Filter 2: reject memory-to-memory transformers (codecs,
            // some ISP pipelines).  These set VIDEO_CAPTURE but exist
            // to *process* frames pushed in via an OUTPUT queue — they
            // never produce frames spontaneously, so FFmpeg trying to
            // capture from them dies immediately with EINVAL.
            const M2M_MASK: u32 = V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE;
            if v4l_caps.capabilities & M2M_MASK != 0 {
                tracing::debug!(
                    "{}: memory-to-memory device (driver={}, caps=0x{:08x}), skipping",
                    path, v4l_caps.driver, v4l_caps.capabilities
                );
                continue;
            }

            // Filter 3: reject specific known non-camera drivers.
            // The Pi's bcm2835-isp registers single-direction capture
            // nodes (so the M2M check above doesn't catch them) but
            // they're still ISP processing nodes and not real camera
            // sources — the user hits this even with no CSI camera
            // attached, because the driver loads by default.
            if NON_CAMERA_DRIVER_PREFIXES
                .iter()
                .any(|prefix| v4l_caps.driver.starts_with(prefix))
            {
                tracing::debug!(
                    "{}: blacklisted driver '{}' (not a real camera source), skipping",
                    path, v4l_caps.driver
                );
                continue;
            }

            // Filter 4: dedup by bus_info.  A single physical camera
            // can expose more than one capture-capable node (e.g. an
            // ISP variant alongside the raw sensor) — without this,
            // those would all show up as separate cameras and FFmpeg
            // would race to open the same hardware.
            //
            // Empty bus_info (rare — non-USB virtual devices) is
            // treated as unique so we don't accidentally dedup
            // unrelated v4l2loopback / vivid devices into one.
            if !v4l_caps.bus_info.is_empty()
                && !seen_bus_info.insert(v4l_caps.bus_info.clone())
            {
                tracing::debug!(
                    "{}: duplicate node for camera at bus '{}' (already enumerated), skipping",
                    path, v4l_caps.bus_info
                );
                continue;
            }

            match probe_camera(&path) {
                Ok(camera) => {
                    tracing::info!(
                        "Detected camera: {} at {} (bus={})",
                        camera.name, camera.device_path, v4l_caps.bus_info
                    );
                    cameras.push(camera);
                }
                Err(e) => {
                    tracing::debug!("Skipping {}: {}", path, e);
                }
            }
        }

        tracing::info!("Found {} camera(s)", cameras.len());
        Ok(cameras)
    }

    fn platform_name(&self) -> &'static str {
        "Linux (v4l2)"
    }
}

/// Probe a single camera device for descriptive metadata.
///
/// Capture-vs-metadata classification and physical-camera dedup
/// both happen in [`LinuxDetector::detect_cameras`] before this is
/// called — so by the time we get here, the device is already known
/// to support `V4L2_CAP_VIDEO_CAPTURE` and to be a unique camera.
fn probe_camera(device_path: &str) -> Result<DetectedCamera> {
    // Get camera name from sysfs
    let name = get_device_name(device_path)?;

    // Try common resolutions to find supported ones
    let supported_resolutions = get_supported_resolutions(device_path);

    // Choose preferred resolution (prefer 1080p, then 720p, then 480p)
    let preferred_resolution = choose_preferred_resolution(&supported_resolutions);

    Ok(DetectedCamera {
        device_path: device_path.to_string(),
        name,
        capabilities: CameraCapabilities {
            streaming: true,
            hardware_encoding: check_hardware_encoding(device_path),
            formats: vec!["YUYV".to_string(), "MJPG".to_string()],
        },
        supported_resolutions,
        preferred_resolution,
    })
}

/// Get camera name from sysfs
fn get_device_name(device_path: &str) -> Result<String> {
    // Extract video number from device path
    let video_num = device_path
        .trim_start_matches("/dev/video")
        .parse::<u32>()
        .map_err(|_| crate::error::Error::Camera("Invalid device path".into()))?;

    // Read name from sysfs
    let sysfs_path = format!("/sys/class/video4linux/video{}/name", video_num);

    let name = match fs::read_to_string(&sysfs_path) {
        Ok(content) => content.trim().to_string(),
        Err(e) => {
            tracing::debug!("Cannot read {}: {}", sysfs_path, e);
            format!("USB Camera {}", video_num)
        }
    };

    Ok(name)
}

// Note: the previous sysfs-based `is_capture_device` (which read
// `/sys/class/video4linux/videoN/dev_caps` and looked for the literal
// string "capture") was removed.  That file isn't a stable kernel
// export — when missing it silently fell through to `Ok(true)` and
// accepted every node, including the metadata sibling, which is what
// caused the duplicate-camera bug on the Pi.  The real check now
// lives in `query_v4l2_caps` and runs against `VIDIOC_QUERYCAP`.

/// Get supported resolutions for a camera
fn get_supported_resolutions(_device_path: &str) -> Vec<(u32, u32)> {
    // Common USB camera resolutions
    // In a full implementation, we'd query this from the device
    vec![
        (1920, 1080), // 1080p
        (1280, 720),  // 720p
        (640, 480),   // VGA
        (320, 240),   // QVGA
    ]
}

/// Choose the best resolution from supported list
fn choose_preferred_resolution(supported: &[(u32, u32)]) -> (u32, u32) {
    // Priority: 1080p > 720p > 480p
    let preferred_order = [(1920, 1080), (1280, 720), (640, 480)];

    for &res in &preferred_order {
        if supported.contains(&res) {
            return res;
        }
    }

    // Fall back to first supported, or 720p default
    supported.first().copied().unwrap_or((1280, 720))
}

/// Check if device supports hardware encoding
fn check_hardware_encoding(_device_path: &str) -> bool {
    // Check for hardware H.264 encoder (common on Raspberry Pi cameras)
    // For now, assume false for USB cameras
    // Raspberry Pi camera module would have this enabled
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel uABI for `struct v4l2_capability` is 104 bytes:
    /// 16 + 32 + 32 + 4 + 4 + 4 + 12.  If anyone touches the struct
    /// definition above and the size moves, VIDIOC_QUERYCAP's ioctl
    /// number (encoded with size=104) becomes wrong and the ioctl
    /// will silently fail with EINVAL on some kernels — or worse,
    /// scribble past the buffer.  Lock it down.
    #[test]
    fn v4l2_capability_struct_matches_kernel_uabi() {
        assert_eq!(std::mem::size_of::<V4l2Capability>(), 104);
    }

    /// Sanity-check the VIDIOC_QUERYCAP magic number — derived
    /// from `_IOR('V', 0, struct v4l2_capability)`:
    ///   dir(2) << 30 | size(104) << 16 | type(0x56) << 8 | nr(0)
    /// = 0x80685600.
    #[test]
    fn vidioc_querycap_value_is_correct() {
        let dir: u64 = 2;
        let size: u64 = std::mem::size_of::<V4l2Capability>() as u64;
        let typ: u64 = b'V' as u64;
        let nr: u64 = 0;
        let expected = (dir << 30) | (size << 16) | (typ << 8) | nr;
        assert_eq!(VIDIOC_QUERYCAP as u64, expected);
        assert_eq!(VIDIOC_QUERYCAP as u64, 0x8068_5600);
    }

    /// `trim_kernel_cstr` is the only thing standing between us and
    /// trailing NULs leaking into log lines and prefix matches.
    #[test]
    fn trim_kernel_cstr_strips_nuls() {
        assert_eq!(trim_kernel_cstr(b"uvcvideo\0\0\0\0\0\0\0\0"), "uvcvideo");
        assert_eq!(trim_kernel_cstr(b"\0\0\0\0"), "");
        // No NUL anywhere — return the whole buffer as-is.
        assert_eq!(trim_kernel_cstr(b"no-nul-here"), "no-nul-here");
        // Real-world bcm2835-isp driver name with kernel padding.
        assert_eq!(trim_kernel_cstr(b"bcm2835-isp\0\0\0\0\0"), "bcm2835-isp");
    }

    /// The Raspberry Pi blacklist is the entire defense against the
    /// "phantom 3rd camera" bug.  Pin the list so a future cleanup
    /// can't quietly drop entries.
    #[test]
    fn non_camera_driver_blacklist_includes_pi_internals() {
        assert!(NON_CAMERA_DRIVER_PREFIXES.contains(&"bcm2835-isp"));
        assert!(NON_CAMERA_DRIVER_PREFIXES.contains(&"bcm2835-codec"));
        // And a positive case: real USB camera drivers must NOT be
        // blacklisted, even though they share no common prefix.
        for good in &["uvcvideo", "unicam", "bcm2835-unicam", "v4l2 loopback"] {
            assert!(
                !NON_CAMERA_DRIVER_PREFIXES
                    .iter()
                    .any(|p| good.starts_with(p)),
                "driver {good:?} must not match any blacklisted prefix"
            );
        }
    }

    #[test]
    fn test_detect_cameras_runs() {
        // This will only work on Linux with actual cameras
        // On other platforms, it should return an empty vector
        let detector = LinuxDetector::new();
        let result = detector.detect_cameras();
        // Just ensure it doesn't panic
        assert!(result.is_ok());
    }

    #[test]
    fn test_choose_preferred_resolution() {
        let supported = vec![(1920, 1080), (1280, 720), (640, 480)];
        let res = choose_preferred_resolution(&supported);
        assert_eq!(res, (1920, 1080));

        let supported = vec![(640, 480)];
        let res = choose_preferred_resolution(&supported);
        assert_eq!(res, (640, 480));

        let supported: Vec<(u32, u32)> = vec![];
        let res = choose_preferred_resolution(&supported);
        assert_eq!(res, (1280, 720)); // Default
    }
}
