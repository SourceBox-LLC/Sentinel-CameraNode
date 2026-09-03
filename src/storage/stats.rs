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

//! Filesystem-aware storage stats.
//!
//! `StorageStats` is the snapshot CloudNode reports to Command Center on
//! every heartbeat: how much of the configured cap (`max_size_gb`) we've
//! used, plus how much free / total space the underlying filesystem has.
//! Command Center renders a per-node usage bar from these numbers and
//! warns the operator if the host disk itself is filling up.
//!
//! There's also a hard-floor recording-pause that fires when the host
//! filesystem drops below `SAFETY_FLOOR_BYTES` — independent of the
//! operator's `max_size_gb`.  The cap protects CloudNode against
//! growing past its allocation; the safety floor protects the *host*
//! against CloudNode filling its disk.  Without the floor, an operator
//! who sets `max_size_gb` larger than the disk can hold will eventually
//! hit `ENOSPC` and SQLite (or worse, every other process on the box)
//! breaks.
//!
//! The pause is a process-wide AtomicBool so the recording writer can
//! check it on the hot path without taking a lock.  Updated by
//! `StorageStats::collect` which runs from the heartbeat task tick
//! (every 30s by default).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sysinfo::Disks;

/// Free-space threshold below which CloudNode pauses new recording writes.
/// 1 GiB is enough headroom that SQLite WAL flushes, FFmpeg HLS rotation,
/// and other normal operations can still finish without `ENOSPC`. Tune
/// upwards on systems with bigger working sets; never below ~256 MiB.
pub const SAFETY_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

/// Set to `true` whenever the most recent `StorageStats::collect` saw
/// disk-free below the safety floor.  The recording writer reads this
/// before each insert; if `true`, it skips the write and logs.
///
/// Process-wide static so any writer can read without plumbing a
/// reference through every call site.  Atomic so reads are lock-free
/// on the hot path.
pub static RECORDING_PAUSED_FOR_DISK: AtomicBool = AtomicBool::new(false);

/// Snapshot of node storage state, collected each heartbeat tick.
///
/// Wire-shaped: serializes directly into the `storage_stats` field of
/// `HeartbeatRequest` so Command Center can persist + display it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StorageStats {
    /// Bytes the node has stored in its SQLite DB (snapshots +
    /// recording_segments tables — same source as `enforce_retention`).
    /// This is the numerator of the operator-visible usage bar.
    pub used_bytes: u64,

    /// Operator-configured cap from `config.storage.max_size_gb`,
    /// converted to bytes.  Denominator of the usage bar.
    pub max_bytes: u64,

    /// Free bytes on the filesystem that holds the data dir.  Used by
    /// Command Center to render a "host disk almost full" warning
    /// alongside the cap-based bar.  0 if we couldn't identify the disk
    /// (Command Center should hide the warning in that case rather than
    /// claim "0 bytes free").
    pub disk_free_bytes: u64,

    /// Total bytes on the same filesystem.  Lets the dashboard show
    /// "X GB free of Y GB" without a second round-trip.
    pub disk_total_bytes: u64,
}

impl StorageStats {
    /// Build a fresh snapshot.  Pulls used-bytes from the caller (cheap,
    /// already cached in the retention path), reads disk stats via
    /// `sysinfo`, and updates the global recording-pause flag as a side
    /// effect so the writers see the new state immediately.
    pub fn collect(used_bytes: u64, max_bytes: u64, data_dir: &Path) -> Self {
        let (disk_free_bytes, disk_total_bytes) = read_disk_info(data_dir);
        let stats = Self {
            used_bytes,
            max_bytes,
            disk_free_bytes,
            disk_total_bytes,
        };

        // Pause recording when the host disk is critically low — but
        // only if we actually know the free space.  A 0 here means
        // "couldn't identify the disk" (no mount-point matched the
        // data_dir) and pausing on that would leave a lot of nodes
        // stuck for no good reason.
        if disk_free_bytes > 0 {
            RECORDING_PAUSED_FOR_DISK.store(
                disk_free_bytes < SAFETY_FLOOR_BYTES,
                Ordering::Relaxed,
            );
        }

        stats
    }

    /// Cap-based usage percentage (0.0–100.0+).  Goes above 100 briefly
    /// between when the writer adds a segment and when retention fires;
    /// callers should clamp for display.
    pub fn percent_full(&self) -> f64 {
        if self.max_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.max_bytes as f64) * 100.0
    }
}

/// Cheap O(disk_count) lookup of the filesystem free + total bytes for
/// the disk that contains `data_dir`.
///
/// Strategy: enumerate every mounted disk and pick the one whose mount
/// point is the *longest* prefix of `data_dir`.  Handles nested mounts
/// like `/var` mounted under `/`, where both match the prefix but
/// `/var` is more specific.  Returns (0, 0) if no disk matches —
/// expected on minimal Docker images that show a virtual rootfs but
/// don't expose a backing block device through `/proc`.
///
/// Windows note: `Path::canonicalize` returns a verbatim/extended-length
/// path (`\\?\C:\Foo`).  sysinfo reports mount points without that
/// prefix (`C:\`), and `Path::starts_with` is component-based — so the
/// two won't match even though they refer to the same drive.  We strip
/// the verbatim prefix before comparing.  Without this strip every
/// Windows node reports `(0, 0)` and the safety floor never trips.
fn read_disk_info(data_dir: &Path) -> (u64, u64) {
    let canonical = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let abs = strip_verbatim_prefix(canonical);

    let disks = Disks::new_with_refreshed_list();
    let mut best: Option<&sysinfo::Disk> = None;
    let mut best_len: usize = 0;

    for disk in &disks {
        let mp = disk.mount_point();
        if abs.starts_with(mp) {
            // `len()` of the OsStr is fine as a "more-specific" tiebreaker
            // since any longer mount point that's also a prefix is by
            // definition deeper in the tree.
            let len = mp.as_os_str().len();
            if len >= best_len {
                best_len = len;
                best = Some(disk);
            }
        }
    }

    match best {
        Some(d) => (d.available_space(), d.total_space()),
        None => (0, 0),
    }
}

/// True if recording writers should skip new writes due to host-disk
/// pressure.  Cheap atomic read; safe to call on the hot path.
pub fn should_pause_recording() -> bool {
    RECORDING_PAUSED_FOR_DISK.load(Ordering::Relaxed)
}

/// Strip the Windows verbatim/extended-length prefix (`\\?\`) from a
/// path so structural prefix comparison against sysinfo mount points
/// works.  No-op on non-Windows.  Leaves UNC verbatim paths alone
/// (`\\?\UNC\server\share` is a network share and won't match a local
/// mount point anyway).
#[cfg(windows)]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        // Non-UTF8 path — leave it alone, the prefix-matching loop will
        // just fail to match and we'll return (0, 0) as before.
        None => return p,
    };
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC\\") {
            return PathBuf::from(rest);
        }
    }
    p
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    p
}

/// Read the host filesystem's free + total bytes for the disk that
/// holds `data_dir`.  Returns `(free_bytes, total_bytes)`, or
/// `(0, 0)` when sysinfo can't identify the disk (Docker rootfs,
/// FUSE).  Public so the setup wizard can compute a sane
/// `max_size_gb` default at install time without spinning up the
/// full StorageStats machinery.
pub fn disk_info(data_dir: &Path) -> (u64, u64) {
    read_disk_info(data_dir)
}

/// Suggest a storage cap in GB based on the disk's currently free
/// space.  Used by the setup wizard as the default value for the
/// operator-confirmable cap prompt.
///
/// Logic: prefer the historical 64 GB default when there's plenty
/// of room.  When the disk has less than 64 GB free, scale down to
/// 80% of free so retention has headroom (the retention loop runs
/// every 5 min — between ticks the writer can sneak past the cap,
/// and a cap = 100% of free would mean ENOSPC every time).  Never
/// suggest below 5 GB; on a tiny disk the operator gets a soft
/// floor to prevent recording from being effectively useless.
///
/// Returns `None` when disk info is unavailable (Docker rootfs).
/// Caller should fall back to the hardcoded 64 GB default in that
/// case.
pub fn suggested_max_gb(data_dir: &Path) -> Option<u64> {
    let (free, _) = read_disk_info(data_dir);
    suggest_from_free_bytes(free)
}

/// The pure half of [`suggested_max_gb`], split out so the decision
/// logic is testable without depending on the host's mount table.
///
/// The filesystem-dependent half genuinely can't be pinned down in a
/// test: `read_disk_info` matches the longest mount-point prefix, and
/// on a normal Linux host EVERY path — including one that doesn't
/// exist — prefixes to the `/` mount and yields real free space. Only
/// a container whose rootfs sysinfo doesn't enumerate ever produces
/// the `free == 0` case, so a test asserting "nonexistent path → None"
/// passed only on that setup and failed everywhere else.
fn suggest_from_free_bytes(free: u64) -> Option<u64> {
    if free == 0 {
        return None;
    }
    let free_gb = free / GIB_AS_U64;
    // 80% headroom factor.  Caller can always override via the
    // wizard prompt — this is only the default the prompt suggests.
    let suggested = (free_gb * 8) / 10;
    Some(suggested.clamp(5, 64))
}

const GIB_AS_U64: u64 = 1024 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_full_handles_zero_cap() {
        let s = StorageStats {
            used_bytes: 100,
            max_bytes: 0,
            disk_free_bytes: 0,
            disk_total_bytes: 0,
        };
        // Zero-cap (legacy install / misconfigured node) must not panic
        // or divide-by-zero — return 0% so the bar shows empty rather
        // than NaN/Inf.
        assert_eq!(s.percent_full(), 0.0);
    }

    #[test]
    fn percent_full_at_cap_is_100() {
        let s = StorageStats {
            used_bytes: 64 * 1024 * 1024 * 1024,
            max_bytes: 64 * 1024 * 1024 * 1024,
            disk_free_bytes: 100 * 1024 * 1024 * 1024,
            disk_total_bytes: 500 * 1024 * 1024 * 1024,
        };
        assert!((s.percent_full() - 100.0).abs() < 0.001);
    }

    #[test]
    fn percent_full_above_cap_overflows_briefly() {
        // The retention loop runs every 5 min; between ticks the writer
        // can push usage just past the cap.  percent_full reports the
        // raw value; UI clamps for display.
        let s = StorageStats {
            used_bytes: 65 * 1024 * 1024 * 1024,
            max_bytes: 64 * 1024 * 1024 * 1024,
            disk_free_bytes: 100 * 1024 * 1024 * 1024,
            disk_total_bytes: 500 * 1024 * 1024 * 1024,
        };
        assert!(s.percent_full() > 100.0);
    }

    #[test]
    fn suggest_from_free_bytes_returns_none_when_disk_info_unavailable() {
        // The Docker-rootfs case: sysinfo enumerates no matching disk,
        // read_disk_info yields 0, and the caller must fall back to its
        // own hardcoded 64 GB default rather than suggesting 0.
        //
        // Tested against the pure helper rather than through
        // suggested_max_gb(nonexistent_path): mount-prefix matching
        // means a bogus path still resolves to `/` on a normal host, so
        // the old path-based version of this test asserted something
        // only true inside a container.
        assert_eq!(suggest_from_free_bytes(0), None);
    }

    #[test]
    fn suggest_from_free_bytes_applies_80_percent_factor_and_clamps() {
        // 50 GB free → 40 GB suggested (80%), inside the [5, 64] clamp.
        assert_eq!(suggest_from_free_bytes(50 * GIB_AS_U64), Some(40));
        // 500 GB free → 400 GB raw, clamped down to the 64 GB ceiling.
        assert_eq!(suggest_from_free_bytes(500 * GIB_AS_U64), Some(64));
        // 1 GB free → 0 GB raw, clamped up to the 5 GB floor so a
        // nearly-full disk still yields a usable (if optimistic) cap.
        assert_eq!(suggest_from_free_bytes(GIB_AS_U64), Some(5));
    }

    #[test]
    fn suggested_max_gb_real_disk_returns_capped_value() {
        // The path the test runner is on has SOME real disk underneath.
        // We don't know which one, but we know the suggestion MUST
        // succeed — `None` here would mean we lost disk identification
        // for the very disk the binary is running on, which is the
        // exact bug the verbatim-prefix strip fixes on Windows.
        let suggestion = suggested_max_gb(std::path::Path::new("."));
        let gb = suggestion.expect(
            "disk identification failed for `.` — \
             read_disk_info() couldn't match the test runner's disk \
             against any sysinfo mount point. On Windows this usually \
             means strip_verbatim_prefix isn't stripping the `\\\\?\\` \
             prefix that canonicalize() introduced.",
        );
        assert!(gb >= 5, "below safety floor: {} GB", gb);
        assert!(gb <= 64, "above historical cap: {} GB", gb);
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_unwraps_canonicalized_disk_paths() {
        // Sanity-check the helper directly: a canonicalized Windows path
        // (\\?\C:\Foo) must come back out as C:\Foo so structural prefix
        // matching against sysinfo's C:\ mount point works.
        let stripped = strip_verbatim_prefix(
            std::path::PathBuf::from(r"\\?\C:\ProgramData\SourceBoxSentry"),
        );
        assert_eq!(
            stripped,
            std::path::PathBuf::from(r"C:\ProgramData\SourceBoxSentry"),
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_leaves_unc_paths_alone() {
        // \\?\UNC\server\share is a network share — the prefix-match loop
        // won't find a local mount point for it anyway, and stripping
        // would corrupt the path shape, so we leave it as-is.
        let unc = std::path::PathBuf::from(r"\\?\UNC\server\share\foo");
        let stripped = strip_verbatim_prefix(unc.clone());
        assert_eq!(stripped, unc);
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_passthrough_for_normal_paths() {
        let p = std::path::PathBuf::from(r"C:\Users\Foo");
        assert_eq!(strip_verbatim_prefix(p.clone()), p);
    }

    #[test]
    fn collect_with_unknown_disk_does_not_set_pause() {
        // A non-existent path should fall through to (0, 0) for disk
        // info, and that should NOT trip the pause flag — pausing on
        // "couldn't identify the disk" would brick a lot of edge-case
        // installs (Docker, weird FUSE mounts) for no reason.
        RECORDING_PAUSED_FOR_DISK.store(false, Ordering::Relaxed);
        let _stats = StorageStats::collect(
            0,
            64 * 1024 * 1024 * 1024,
            std::path::Path::new("/this/path/does/not/exist/anywhere"),
        );
        assert!(!RECORDING_PAUSED_FOR_DISK.load(Ordering::Relaxed));
    }
}
