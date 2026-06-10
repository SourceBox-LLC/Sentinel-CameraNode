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
//! SQLite-backed local storage for snapshots and recording segments.
//!
//! Replaces flat-file storage so data isn't exposed in open folders.
//! All binary data (JPEG snapshots, TS segments) is stored as encrypted BLOBs
//! using AES-256-GCM with a machine-id-derived key — the same key used for
//! config secrets. A stolen copy of `node.db` alone can't be decrypted
//! anywhere else; the attacker also needs code execution on the original
//! host so they can read `/etc/machine-id` (or the Windows/macOS equivalent).
//!
//! BLOB layout (written by `encrypt_bytes`, since v0.1.18):
//!     [5-byte magic "OSE\x02\x02"][12-byte nonce][ciphertext || 16-byte GCM tag]
//! The magic lets future code cleanly refuse an un-encrypted blob rather
//! than hand raw bytes to the AES-GCM decrypt routine, which would just
//! return a generic "decryption failed" error. The v0.1.18 V2 format binds
//! the row's identity (`(camera_id, kind, identifier)`) into the auth tag
//! as additional authenticated data — see the `snapshot_aad` /
//! `recording_aad` helpers — so a swap-row attack pasting an encrypted
//! blob into a different row's `data` column fails decryption cleanly.
//! The pre-v0.1.18 V1 format ("OSE\x02\x01") is still readable for nodes
//! upgrading in place; new writes always use V2.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::{Error, Result};

/// Thread-safe handle to the local SQLite database.
#[derive(Clone)]
pub struct NodeDatabase {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotRecord {
    pub id: i64,
    pub camera_id: String,
    pub filename: String,
    pub timestamp: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct RecordingSummary {
    pub camera_id: String,
    pub date: String,
    pub segment_count: u64,
    pub total_size_bytes: u64,
}

impl NodeDatabase {
    /// Open (or create) the database at the given path.
    pub fn new(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Storage(format!("Cannot create DB dir: {}", e)))?;
        }

        let conn = Connection::open(db_path)
            .map_err(|e| Error::Storage(format!("Cannot open DB: {}", e)))?;

        conn.execute_batch(
            // busy_timeout comes first: the runner holds ONE shared connection
            // (Arc<Mutex<Connection>>), but config load/save opens its OWN
            // separate connection to the same node.db. Under WAL two writers on
            // different connections collide, and without a busy_timeout SQLite
            // returns SQLITE_BUSY *immediately* — the losing write (e.g. a
            // segment-archive INSERT) is then dropped with only a warn. 5s of
            // retry absorbs the brief contention windows (config writes, and
            // retention's wal_checkpoint(TRUNCATE)/VACUUM).
            //
            // auto_vacuum=INCREMENTAL MUST come before any table is
            // created.  On a fresh DB it's adopted immediately (no
            // VACUUM needed); on an existing DB the setting is inert
            // until a one-time VACUUM rewrites the file (handled below).
            // Without it, `enforce_retention`'s DELETEs free pages to
            // the freelist but never return them to the filesystem —
            // the .db file stays at its high-water mark forever while
            // SUM(size_bytes) (the retention cap accounting) reads low.
            // On a Pi SD card that's a silent disk-fill: the operator
            // sees "usage fine" while the actual file is much larger.
            "PRAGMA busy_timeout=5000;
             PRAGMA auto_vacuum=INCREMENTAL;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;

             CREATE TABLE IF NOT EXISTS snapshots (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 camera_id  TEXT    NOT NULL,
                 filename   TEXT    NOT NULL,
                 timestamp  INTEGER NOT NULL,
                 data       BLOB   NOT NULL,
                 size_bytes INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS recording_segments (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 camera_id   TEXT    NOT NULL,
                 segment_seq INTEGER NOT NULL,
                 date        TEXT    NOT NULL,
                 data        BLOB   NOT NULL,
                 size_bytes  INTEGER NOT NULL,
                 duration_ms INTEGER NOT NULL DEFAULT 1000
             );

             CREATE TABLE IF NOT EXISTS config (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS logs (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT    NOT NULL,
                 level     TEXT    NOT NULL,
                 message   TEXT    NOT NULL
             );

             /* Phase B (local web UI) — per-camera recording toggle for
                Local-mode installs. In Connected mode this table is
                ignored; CC's heartbeat reconciler is the source of
                truth for recording_state. */
             CREATE TABLE IF NOT EXISTS local_recording_state (
                 camera_id TEXT    PRIMARY KEY,
                 enabled   INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_snap_camera
                 ON snapshots(camera_id);
             CREATE INDEX IF NOT EXISTS idx_rec_camera_date
                 ON recording_segments(camera_id, date);
             CREATE INDEX IF NOT EXISTS idx_logs_timestamp
                 ON logs(id DESC);

             /* COVERING indexes for the blob table's routine aggregates.
                size_bytes/duration_ms are stored AFTER the multi-hundred-KB
                data BLOB, and SQLite reaches post-blob columns by walking
                the row's overflow-page chain — so SUM(size_bytes) (the
                heartbeat's storage gauge, every 30s) and the playback
                playlist's MAX(duration_ms) were effectively full reads of
                every archived video byte: minutes of SD-card I/O per tick
                once the archive outgrows RAM.  With these, both queries
                are index-only scans that never touch a blob page.
                IF NOT EXISTS → existing DBs pick them up on next boot. */
             CREATE INDEX IF NOT EXISTS idx_rec_size
                 ON recording_segments(size_bytes);
             CREATE INDEX IF NOT EXISTS idx_rec_playlist
                 ON recording_segments(camera_id, date, segment_seq, duration_ms);",
        )
        .map_err(|e| Error::Storage(format!("DB init error: {}", e)))?;

        // Schema migration for existing DBs: add duration_ms column if it
        // doesn't already exist.  SQLite has no ADD COLUMN IF NOT EXISTS
        // pre-3.35, so just run the ALTER and swallow the duplicate-column
        // error.  The CREATE TABLE above already includes the column for
        // fresh installs.
        if let Err(e) = conn.execute(
            "ALTER TABLE recording_segments ADD COLUMN duration_ms INTEGER NOT NULL DEFAULT 1000",
            [],
        ) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") && !msg.contains("already exists") {
                return Err(Error::Storage(format!("DB migration error: {}", e)));
            }
        }

        // One-time conversion for DBs created before auto_vacuum was set.
        // `PRAGMA auto_vacuum` reads the page-1 header flag, which only
        // changes when VACUUM rewrites the file — so a value != 2
        // (INCREMENTAL) means this is a pre-existing DB that needs the
        // conversion.  A fresh DB already reads 2 (the pragma above took
        // effect before CREATE TABLE), so this VACUUM never runs for new
        // installs.
        //
        // The VACUUM rewrites the whole file and needs free space ~= the
        // DB size — which on a nearly-full SD card (the exact scenario
        // this fix targets) can itself fail.  So it's best-effort: on
        // failure we log and continue with the old layout.  DELETEs
        // still work; `incremental_vacuum` just no-ops until a future
        // boot with enough headroom completes the conversion.  Never let
        // a failed reclamation-conversion brick node startup.
        let auto_vacuum_mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap_or(2);
        if auto_vacuum_mode != 2 {
            match conn.execute_batch("VACUUM;") {
                Ok(()) => tracing::info!(
                    "node.db converted to incremental auto_vacuum — \
                     retention can now reclaim disk space"
                ),
                Err(e) => tracing::warn!(
                    "auto_vacuum conversion VACUUM failed (likely low disk); \
                     retention DELETEs still work, file reclamation will \
                     retry on a future boot with more free space: {}",
                    e
                ),
            }
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ── Snapshots ────────────────────────────────────────────────────────

    /// Insert a snapshot row and return its primary-key id.
    ///
    /// Returning the id directly from `conn.last_insert_rowid()` while
    /// still holding the connection lock is the race-free way to map a
    /// just-inserted row to its id.  The previous flow saved the
    /// snapshot here, then a separate `last_snapshot_id_for(camera_id,
    /// filename, timestamp)` query looked up "the" id — but two
    /// snapshots fired in the same millisecond on the same camera
    /// (HTTP route + WS dispatcher both triggering) end up with
    /// identical `(camera_id, filename, timestamp)` tuples, and the
    /// lookup's `ORDER BY id DESC LIMIT 1` returns the *later* row to
    /// both callers — the earlier caller then fetches the later
    /// caller's bytes.  Using `last_insert_rowid` per-connection
    /// closes that race because SQLite's rowid counter is
    /// connection-local and the lock here is the same lock that
    /// guards the INSERT.
    pub fn save_snapshot(
        &self,
        camera_id: &str,
        filename: &str,
        timestamp: i64,
        data: &[u8],
    ) -> Result<i64> {
        // `size_bytes` records the *plaintext* length — that's what the
        // retention quota and the user-facing "disk usage" display care
        // about. Storing the ciphertext length would drift the retention
        // accounting by ~33 bytes per blob, which compounds across millions
        // of segments and breaks the user's quota intuition.
        let plaintext_len = data.len() as i64;
        // Bind the row's identity into the AES-GCM auth tag so a swap-row
        // attack — pasting this blob into a different snapshot row — fails
        // decryption when the reader recomputes the AAD against the wrong
        // (camera_id, filename, timestamp) tuple. See ADR 0002.
        let aad = snapshot_aad(camera_id, filename, timestamp);
        let encrypted = encrypt_bytes(data, &aad)
            .map_err(|e| Error::Storage(format!("Snapshot encrypt error: {}", e)))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO snapshots (camera_id, filename, timestamp, data, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![camera_id, filename, timestamp, encrypted, plaintext_len],
        )
        .map_err(|e| Error::Storage(format!("Snapshot insert error: {}", e)))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_snapshots(&self, camera_id: Option<&str>) -> Result<Vec<SnapshotRecord>> {
        let conn = self.lock()?;
        let mut rows = Vec::new();

        if let Some(cam) = camera_id {
            let mut stmt = conn
                .prepare(
                    "SELECT id, camera_id, filename, timestamp, size_bytes
                     FROM snapshots WHERE camera_id = ?1 ORDER BY timestamp DESC",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let iter = stmt
                .query_map(params![cam], |row| {
                    Ok(SnapshotRecord {
                        id: row.get(0)?,
                        camera_id: row.get(1)?,
                        filename: row.get(2)?,
                        timestamp: row.get(3)?,
                        size_bytes: row.get::<_, i64>(4)? as u64,
                    })
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            for r in iter {
                rows.push(r.map_err(|e| Error::Storage(e.to_string()))?);
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT id, camera_id, filename, timestamp, size_bytes
                     FROM snapshots ORDER BY timestamp DESC",
                )
                .map_err(|e| Error::Storage(e.to_string()))?;
            let iter = stmt
                .query_map([], |row| {
                    Ok(SnapshotRecord {
                        id: row.get(0)?,
                        camera_id: row.get(1)?,
                        filename: row.get(2)?,
                        timestamp: row.get(3)?,
                        size_bytes: row.get::<_, i64>(4)? as u64,
                    })
                })
                .map_err(|e| Error::Storage(e.to_string()))?;
            for r in iter {
                rows.push(r.map_err(|e| Error::Storage(e.to_string()))?);
            }
        }

        Ok(rows)
    }

    /// Permanently delete a snapshot row by id.  Returns the number of
    /// rows actually removed (0 if the id wasn't found, 1 on success).
    /// Used by the SPA's snapshot gallery's per-row trash button.
    /// Retention-based deletion lives separately in `enforce_retention`.
    pub fn delete_snapshot(&self, id: i64) -> Result<usize> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM snapshots WHERE id = ?1", params![id])
            .map_err(|e| Error::Storage(format!("Snapshot delete error: {}", e)))
    }

    // ── Recordings ───────────────────────────────────────────────────────

    pub fn save_recording_segment(
        &self,
        camera_id: &str,
        segment_seq: u64,
        date: &str,
        data: &[u8],
        duration_ms: u32,
    ) -> Result<()> {
        // See `save_snapshot` for why `size_bytes` is the plaintext length.
        let plaintext_len = data.len() as i64;
        // Bind (camera_id, segment_seq, date) into the auth tag — see the
        // matching comment in `save_snapshot` for the rationale.
        let aad = recording_aad(camera_id, segment_seq, date);
        let encrypted = encrypt_bytes(data, &aad)
            .map_err(|e| Error::Storage(format!("Recording encrypt error: {}", e)))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO recording_segments (camera_id, segment_seq, date, data, size_bytes, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![camera_id, segment_seq as i64, date, encrypted, plaintext_len, duration_ms as i64],
        )
        .map_err(|e| Error::Storage(format!("Recording insert error: {}", e)))?;
        Ok(())
    }

    // ── Phase B (local web UI) accessors ────────────────────────────

    // `last_snapshot_id_for` lived here pre-v0.1.57.  It looked up the
    // just-inserted row by (camera_id, filename, timestamp), but two
    // snapshots fired in the same millisecond on the same camera
    // produced identical tuples and the lookup's `ORDER BY id DESC
    // LIMIT 1` returned the later row to both callers.  `save_snapshot`
    // now returns `last_insert_rowid` directly under the connection
    // lock instead — race-free.  Helper removed; the call site in
    // `api/commands.rs::take_snapshot` uses the new return value.

    /// Decrypt and return the JPEG bytes for a saved snapshot.  The
    /// AAD is recomputed from (camera_id, filename, timestamp) so a
    /// row whose blob got swapped at rest fails decryption.
    pub fn get_snapshot_data(&self, id: i64) -> Result<Vec<u8>> {
        let conn = self.lock()?;
        let (camera_id, filename, timestamp, blob): (String, String, i64, Vec<u8>) = conn
            .query_row(
                "SELECT camera_id, filename, timestamp, data
                 FROM snapshots WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| Error::Storage(format!("Snapshot lookup error: {}", e)))?;
        let aad = snapshot_aad(&camera_id, &filename, timestamp);
        decrypt_bytes(&blob, &aad)
            .map_err(|e| Error::Storage(format!("Snapshot decrypt error: {}", e)))
    }

    /// List `(segment_seq, duration_ms)` for a given camera + date,
    /// ordered ascending so the M3U8 builder can emit segments in
    /// playback order.  Empty Vec when no recordings match.
    ///
    /// De-duplicated by `segment_seq` (GROUP BY): the table has no
    /// UNIQUE(camera_id, date, segment_seq) constraint, and FFmpeg's
    /// `segment_%05d.ts` counter restarts at 0 whenever the supervisor
    /// restarts the pipeline (a flaky USB/RTSP camera on a Pi does this
    /// routinely), so two physically distinct rows can share the same
    /// (camera, date, seq) within one UTC day.  The VOD playlist builder
    /// emits exactly one `segment_<seq>.ts` line per row returned here,
    /// and the fetch route resolves that URL to a single blob — so a
    /// duplicate seq would make the player replay the same ~1s twice
    /// while the colliding segment stays unreachable.  Collapsing to one
    /// row per seq keeps the playlist well-formed.  `MAX(duration_ms)` is
    /// the safe pick for the survivor: TARGETDURATION must bound every
    /// EXTINF, so erring toward the longer duration can't violate the
    /// spec.  (Making *both* colliding blobs individually addressable is
    /// a deeper change tracked separately.)
    pub fn list_recording_segment_seqs(
        &self,
        camera_id: &str,
        date: &str,
    ) -> Result<Vec<(u64, u32)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT segment_seq, MAX(duration_ms) FROM recording_segments
                 WHERE camera_id = ?1 AND date = ?2
                 GROUP BY segment_seq
                 ORDER BY segment_seq ASC",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        let iter = stmt
            .query_map(params![camera_id, date], |row| {
                let seq: i64 = row.get(0)?;
                let dur: i64 = row.get(1)?;
                Ok((seq as u64, dur as u32))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = Vec::new();
        for r in iter {
            out.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Decrypt and return the raw MPEG-TS bytes for a single recording
    /// segment.  Errors when no row matches the (camera_id, date,
    /// segment_seq) tuple — caller should map that to HTTP 404.
    pub fn get_recording_segment(
        &self,
        camera_id: &str,
        date: &str,
        segment_seq: u64,
    ) -> Result<Vec<u8>> {
        let conn = self.lock()?;
        // ORDER BY id DESC — colliding seqs (pre-v0.1.70 FFmpeg-restart
        // duplicates) keep multiple physical rows; the playback playlist
        // advertises MAX(duration_ms), so serve the NEWEST row rather
        // than whichever one the planner happened to hit first.
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT data FROM recording_segments
                 WHERE camera_id = ?1 AND date = ?2 AND segment_seq = ?3
                 ORDER BY id DESC LIMIT 1",
                params![camera_id, date, segment_seq as i64],
                |row| row.get(0),
            )
            .map_err(|e| Error::Storage(format!("Segment lookup error: {}", e)))?;
        let aad = recording_aad(camera_id, segment_seq, date);
        decrypt_bytes(&blob, &aad)
            .map_err(|e| Error::Storage(format!("Segment decrypt error: {}", e)))
    }

    /// Set the local recording flag for one camera (Local-mode only).
    /// In Connected mode this table is ignored; CC's heartbeat
    /// reconciler is the source of truth.
    pub fn set_local_recording(&self, camera_id: &str, enabled: bool) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO local_recording_state (camera_id, enabled)
             VALUES (?1, ?2)
             ON CONFLICT(camera_id) DO UPDATE SET enabled = excluded.enabled",
            params![camera_id, if enabled { 1 } else { 0 }],
        )
        .map_err(|e| Error::Storage(format!("Local recording write error: {}", e)))?;
        Ok(())
    }

    /// Read the entire local recording-state map.  Called once at boot
    /// to seed the in-memory `recording_state` HashSet, so a Local-mode
    /// node remembers per-camera record toggles across restarts.
    pub fn get_local_recording_state(&self) -> Result<std::collections::HashMap<String, bool>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT camera_id, enabled FROM local_recording_state")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let iter = stmt
            .query_map([], |row| {
                let cam: String = row.get(0)?;
                let enabled: i64 = row.get(1)?;
                Ok((cam, enabled != 0))
            })
            .map_err(|e| Error::Storage(e.to_string()))?;
        let mut out = std::collections::HashMap::new();
        for r in iter {
            let (k, v) = r.map_err(|e| Error::Storage(e.to_string()))?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn list_recordings(
        &self,
        camera_id: Option<&str>,
    ) -> Result<Vec<RecordingSummary>> {
        let conn = self.lock()?;
        let mut rows = Vec::new();

        let sql = if camera_id.is_some() {
            "SELECT camera_id, date, COUNT(*) as cnt, SUM(size_bytes) as total
             FROM recording_segments WHERE camera_id = ?1
             GROUP BY camera_id, date ORDER BY date DESC"
        } else {
            "SELECT camera_id, date, COUNT(*) as cnt, SUM(size_bytes) as total
             FROM recording_segments
             GROUP BY camera_id, date ORDER BY date DESC"
        };

        let mut stmt = conn.prepare(sql).map_err(|e| Error::Storage(e.to_string()))?;

        let iter = if let Some(cam) = camera_id {
            stmt.query_map(params![cam], Self::map_recording_summary)
        } else {
            stmt.query_map([], Self::map_recording_summary)
        }
        .map_err(|e| Error::Storage(e.to_string()))?;

        for r in iter {
            rows.push(r.map_err(|e| Error::Storage(e.to_string()))?);
        }
        Ok(rows)
    }

    fn map_recording_summary(row: &rusqlite::Row) -> rusqlite::Result<RecordingSummary> {
        Ok(RecordingSummary {
            camera_id: row.get(0)?,
            date: row.get(1)?,
            segment_count: row.get::<_, i64>(2)? as u64,
            total_size_bytes: row.get::<_, i64>(3)? as u64,
        })
    }

    // ── Retention ────────────────────────────────────────────────────────

    /// Total bytes stored across snapshots + recordings.
    pub fn total_size(&self) -> Result<u64> {
        let conn = self.lock()?;
        let snap: i64 = conn
            .query_row("SELECT COALESCE(SUM(size_bytes),0) FROM snapshots", [], |r| r.get(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rec: i64 = conn
            .query_row("SELECT COALESCE(SUM(size_bytes),0) FROM recording_segments", [], |r| r.get(0))
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok((snap + rec) as u64)
    }

    /// Delete the oldest data until total size is under `max_bytes`.
    /// Returns `(current_size, bytes_freed)`.
    pub fn enforce_retention(&self, max_bytes: u64) -> Result<(u64, u64)> {
        let total = self.total_size()?;
        if total <= max_bytes {
            return Ok((total, 0));
        }

        let conn = self.lock()?;
        let mut freed: u64 = 0;
        let excess = total - max_bytes;

        // Delete oldest recording segments first (they're the bulk of the data)
        {
            let mut stmt = conn
                .prepare("SELECT id, size_bytes FROM recording_segments ORDER BY id ASC")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows: Vec<(i64, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| Error::Storage(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();

            for (id, size) in rows {
                if freed >= excess {
                    break;
                }
                conn.execute("DELETE FROM recording_segments WHERE id = ?1", params![id])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                freed += size as u64;
            }
        }

        // If still over, delete oldest snapshots
        if freed < excess {
            let mut stmt = conn
                .prepare("SELECT id, size_bytes FROM snapshots ORDER BY id ASC")
                .map_err(|e| Error::Storage(e.to_string()))?;
            let rows: Vec<(i64, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| Error::Storage(e.to_string()))?
                .filter_map(|r| r.ok())
                .collect();

            for (id, size) in rows {
                if freed >= excess {
                    break;
                }
                conn.execute("DELETE FROM snapshots WHERE id = ?1", params![id])
                    .map_err(|e| Error::Storage(e.to_string()))?;
                freed += size as u64;
            }
        }

        // Actually hand the freed pages back to the filesystem.  The
        // DELETEs above only move pages to SQLite's freelist; without
        // this the .db file never shrinks (the whole point of the
        // retention cap on a space-constrained Pi).  `incremental_vacuum`
        // returns freelist pages to the OS (no-op until the auto_vacuum
        // conversion in `new()` has run); `wal_checkpoint(TRUNCATE)`
        // then shrinks the -wal sidecar, which otherwise pins the space
        // we just reclaimed.  Best-effort — a failure here doesn't undo
        // the committed DELETEs, it just defers reclamation to the next
        // 5-minute retention tick.
        if freed > 0 {
            if let Err(e) = conn.execute_batch(
                "PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);",
            ) {
                tracing::warn!(
                    "post-retention reclamation (incremental_vacuum / wal \
                     checkpoint) failed; space frees on the next pass: {}",
                    e
                );
            }
        }

        Ok((total - freed, freed))
    }

    // ── Logs ─────────────────────────────────────────────────────────────

    /// Persist a single log entry.
    pub fn save_log(&self, timestamp: &str, level: &str, message: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO logs (timestamp, level, message) VALUES (?1, ?2, ?3)",
            params![timestamp, level, message],
        )
        .map_err(|e| Error::Storage(format!("Log insert error: {}", e)))?;
        Ok(())
    }

    /// Load the most recent `limit` log entries (oldest first).
    pub fn load_recent_logs(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT timestamp, level, message FROM logs ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| Error::Storage(e.to_string()))?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| Error::Storage(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();
        // Reverse so oldest is first (for VecDeque push_back ordering)
        Ok(rows.into_iter().rev().collect())
    }

    /// Keep only the most recent `keep` log entries, delete the rest.
    /// Returns the number of rows deleted.
    pub fn prune_logs(&self, keep: usize) -> Result<u64> {
        let conn = self.lock()?;
        let deleted = conn
            .execute(
                "DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT ?1)",
                params![keep as i64],
            )
            .map_err(|e| Error::Storage(format!("Log prune error: {}", e)))?;
        Ok(deleted as u64)
    }

    // ── Config ───────────────────────────────────────────────────────────

    /// Store a config value (plaintext).
    pub fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO config (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .map_err(|e| Error::Storage(format!("Config set error: {}", e)))?;
        Ok(())
    }

    /// Read a config value (plaintext).
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT value FROM config WHERE key = ?1")
            .map_err(|e| Error::Storage(e.to_string()))?;
        let result = stmt
            .query_row(params![key], |row| row.get(0))
            .ok();
        Ok(result)
    }

    /// Store a config value encrypted with machine-derived key.
    pub fn set_config_encrypted(&self, key: &str, plaintext: &str) -> Result<()> {
        let encrypted = encrypt_value(plaintext)
            .map_err(|e| Error::Storage(format!("Encryption error: {}", e)))?;
        self.set_config(key, &encrypted)
    }

    /// Read and decrypt a config value.
    ///
    /// If the ciphertext was written by a CloudNode version that derived its
    /// key from the hostname, the value is transparently re-encrypted with
    /// the current machine-id-derived key so subsequent loads take the fast
    /// path and the weak legacy key is retired from the DB.
    pub fn get_config_encrypted(&self, key: &str) -> Result<Option<String>> {
        match self.get_config(key)? {
            Some(encrypted) => {
                let (plaintext, was_legacy) = decrypt_value(&encrypted)
                    .map_err(|e| Error::Storage(format!("Decryption error: {}", e)))?;
                if was_legacy {
                    tracing::info!(
                        "Migrating encrypted config key '{}' from hostname-derived \
                         (v1) to machine-id-derived (v2) encryption key",
                        key,
                    );
                    // Best-effort: if re-encryption fails we still return the
                    // decrypted value so the node keeps working. Next startup
                    // will retry the migration.
                    if let Err(e) = self.set_config_encrypted(key, &plaintext) {
                        tracing::warn!(
                            "Failed to re-encrypt config key '{}' during migration: {}",
                            key,
                            e,
                        );
                    }
                }
                Ok(Some(plaintext))
            }
            None => Ok(None),
        }
    }

    /// Delete a config key from the database.
    pub fn delete_config(&self, key: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM config WHERE key = ?1", params![key])
            .map_err(|e| Error::Storage(format!("Config delete error: {}", e)))?;
        Ok(())
    }

    /// Check if the DB looks "configured" — i.e. the wizard has run.
    ///
    /// Two configured shapes count as a yes:
    ///   - `mode='local'` row present (Local-mode install — by design
    ///     no `node_id` / `api_key` are written), OR
    ///   - `node_id` row present (Connected-mode install).
    ///
    /// A bare DB with neither falls through to the YAML/env loader
    /// path so a stale `data/node.db` next to a `config.yaml` doesn't
    /// shadow the YAML.
    pub fn has_config(&self) -> bool {
        if let Ok(Some(_)) = self.get_config("mode") {
            return true;
        }
        self.get_config("node_id").ok().flatten().is_some()
    }

    // ── Wipe ─────────────────────────────────────────────────────────────

    /// Delete all stored data (called when the node is decommissioned).
    pub fn wipe_all(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            "DELETE FROM snapshots; DELETE FROM recording_segments; DELETE FROM logs; DELETE FROM config; VACUUM;",
        )
        .map_err(|e| Error::Storage(format!("Wipe error: {}", e)))?;
        Ok(())
    }

    // ── Internal ─────────────────────────────────────────────────────────

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| Error::Storage(format!("DB lock error: {}", e)))
    }
}

// ─── AES-256-GCM encryption ─────────────────────────────────────────────────
//
// Secrets (the cloud API key and every recording/snapshot BLOB) are encrypted
// at rest with a key derived from the host machine's OS-managed machine
// identifier — on Linux `/etc/machine-id`, on Windows `HKLM\SOFTWARE\Microsoft
// \Cryptography\MachineGuid`, on macOS `IOPlatformUUID`. These are 128-bit
// values set once at OS install time and not user-modifiable, so an attacker
// who steals just `node.db` can't decrypt anything without also having code
// execution on the original host.
//
// Two ciphertext shapes are supported:
//   - Config strings use hex-encoded `nonce ‖ ciphertext` written via
//     `encrypt_value` / `decrypt_value` (the older path).
//   - BLOBs use a 5-byte magic prefix followed by raw
//     `nonce ‖ ciphertext` via `encrypt_bytes` / `decrypt_bytes`. Current
//     writes use the v0.1.18+ V2 format with prefix `OSE\x02\x02` and AAD
//     bound to the row's identity tuple (see `BLOB_MAGIC_V2` + `*_aad()`
//     helpers below); the pre-v0.1.18 V1 prefix `OSE\x02\x01` (no AAD) is
//     still readable for back-compat. Hex would double the on-disk
//     footprint of multi-megabyte video segments; the magic prefix lets a
//     future reader cleanly reject an unencrypted blob rather than pass
//     raw bytes to AES-GCM and get a generic tag-failure.
//
// Earlier versions derived the key from the hostname, which is predictable
// (most Pis ship as `raspberrypi`) and trivially guessable from a stolen DB.
// The older path is kept as `derive_key_legacy` only to transparently migrate
// existing installs: on first decrypt we try the new key, fall back to the
// legacy key, and immediately re-encrypt with the new key.

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, AeadCore, Nonce,
};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Domain-separation tag mixed into the SHA-256 hash so the same machine ID
/// never produces the same key as some unrelated tool that happens to hash
/// the same input. `v2` marks the switch from hostname-derived (v1) keys.
///
/// **Do not rename across brand changes.**  This string is part of the key
/// derivation; changing it would brick every existing install's encrypted
/// SQLite DB.  The `opensentry-` prefix is a historical artifact from
/// when the product was called "OpenSentry" — kept verbatim through the
/// later "SourceBox Sentry" and "Sentinel by SourceBox" rebrands so
/// nodes can still decrypt their data after an upgrade.
const KEY_DOMAIN_V2: &[u8] = b"opensentry-cloudnode-machine-id-v2";

/// Legacy domain tag — kept so `derive_key_legacy` still reproduces the
/// pre-migration key for DBs written with the old code.  Same brand-
/// history note as `KEY_DOMAIN_V2`: this string is load-bearing for
/// decryption, not a brand reference.
const KEY_DOMAIN_V1_LEGACY: &[u8] = b"opensentry-cloudnode-v1";

/// Cached derived key for this process — `machine_id()` on Linux reads a
/// file and on Windows / macOS shells out, so we avoid re-deriving per op.
static CACHED_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Read the OS-managed machine identifier.
///
/// Returns an error (not a fallback value) so weak encryption can't silently
/// slip back in — the caller surfaces the error and the user can file a bug
/// with the exact platform. Cached by `derive_key` so we pay the I/O cost at
/// most once per process.
///
/// On Linux, if the OS sources are unavailable (e.g. minimal Docker images
/// without systemd or dbus), falls back to a node-local identifier stored at
/// `$SOURCEBOX_SENTRY_DATA_DIR/.machine-id`, generating one from
/// `/proc/sys/kernel/random/uuid` on first use. This is weaker than a
/// host-wide ID (an attacker who copies the data directory gets the key
/// material) but still a major upgrade over the hostname-derived v1 key.
fn machine_id() -> std::result::Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        // systemd / freedesktop.org standard. Both files hold a 32-char hex
        // string written at OS install time.
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(content) = std::fs::read_to_string(path) {
                let id = content.trim();
                if !id.is_empty() {
                    return Ok(id.to_string());
                }
            }
        }
        // Docker/minimal-image fallback: generate once, persist to the data
        // volume so the ID survives container rebuilds.
        if let Ok(data_dir) = std::env::var("SOURCEBOX_SENTRY_DATA_DIR") {
            let fallback_path = std::path::PathBuf::from(&data_dir).join(".machine-id");
            if let Ok(content) = std::fs::read_to_string(&fallback_path) {
                let id = content.trim();
                if !id.is_empty() {
                    return Ok(id.to_string());
                }
            }
            // Not present — generate from the kernel RNG.
            if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
                let id = uuid.trim().to_string();
                if !id.is_empty() {
                    // Best-effort write; if it fails we still return the ID
                    // for this process but the next run will generate again.
                    if let Err(e) = std::fs::write(&fallback_path, &id) {
                        tracing::warn!(
                            "Could not persist fallback machine ID to {}: {}",
                            fallback_path.display(),
                            e,
                        );
                    }
                    return Ok(id);
                }
            }
        }
        return Err(
            "machine ID not found (tried /etc/machine-id, /var/lib/dbus/machine-id, \
             $SOURCEBOX_SENTRY_DATA_DIR/.machine-id). \
             Run `sudo systemd-machine-id-setup` or `sudo dbus-uuidgen --ensure=/etc/machine-id`, \
             or set SOURCEBOX_SENTRY_DATA_DIR to a writable directory."
                .into(),
        );
    }

    #[cfg(target_os = "windows")]
    {
        // HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid — present on every
        // Windows install since Vista. Read via `reg query` so we don't pull
        // in a Windows-only registry crate.
        //
        // Use the absolute path under %SystemRoot%\System32 instead of
        // relying on PATH resolution.  PATH-based resolution can be hijacked
        // if a malicious `reg.exe` is dropped earlier in the search order
        // (cwd, %USERPROFILE%, etc.); since this call returns the master
        // value used to derive the AES key for the on-disk config, a
        // compromised `reg.exe` would let an attacker control the encryption
        // key.  Resolving %SystemRoot% via env var (not hardcoded
        // C:\Windows\) keeps it robust against unusual Windows installs
        // that put the OS on a non-default drive.
        use std::process::Command;
        let system_root = std::env::var("SystemRoot")
            .unwrap_or_else(|_| r"C:\Windows".to_string());
        let reg_exe = format!(r"{}\System32\reg.exe", system_root);
        let output = Command::new(&reg_exe)
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ])
            .output()
            .map_err(|e| format!("{} failed: {}", reg_exe, e))?;
        if !output.status.success() {
            return Err(format!(
                "reg query exited with status {}",
                output.status
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Output format: "    MachineGuid    REG_SZ    <guid>"
        for line in stdout.lines() {
            if let Some(idx) = line.find("REG_SZ") {
                let value = line[idx + "REG_SZ".len()..].trim();
                if !value.is_empty() {
                    return Ok(value.to_string());
                }
            }
        }
        return Err("MachineGuid not found in registry output".into());
    }

    #[cfg(target_os = "macos")]
    {
        // IOPlatformUUID from the IOKit registry.
        //
        // Same threat model as the Windows reg.exe lookup above:
        // PATH-based binary resolution can be hijacked, and this call
        // produces the master value used to derive the AES key for
        // the on-disk config.  Use the canonical absolute path
        // /usr/sbin/ioreg (stable since Mac OS X 10.0) rather than
        // letting `$PATH` decide which `ioreg` to invoke.
        use std::process::Command;
        let output = Command::new("/usr/sbin/ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .map_err(|e| format!("/usr/sbin/ioreg failed: {}", e))?;
        if !output.status.success() {
            return Err(format!("ioreg exited with status {}", output.status));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("IOPlatformUUID") {
                // Line format: `    "IOPlatformUUID" = "XXXXXXXX-..."`
                if let Some(after_eq) = line.split('=').nth(1) {
                    let value = after_eq.trim().trim_matches('"');
                    if !value.is_empty() {
                        return Ok(value.to_string());
                    }
                }
            }
        }
        return Err("IOPlatformUUID not found in ioreg output".into());
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err("machine ID lookup not implemented for this platform".into())
    }
}

/// Derive a 256-bit encryption key from the OS machine identifier.
fn derive_key() -> std::result::Result<[u8; 32], String> {
    if let Some(k) = CACHED_KEY.get() {
        return Ok(*k);
    }
    let id = machine_id()?;
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    hasher.update(KEY_DOMAIN_V2);
    let key: [u8; 32] = hasher.finalize().into();
    // OnceLock::set returns Err if already set — harmless race, either value
    // is the same because machine_id() is deterministic within a process.
    let _ = CACHED_KEY.set(key);
    Ok(key)
}

/// Derive the pre-migration key from the hostname.
///
/// Only used by `decrypt_value` to transparently migrate DBs written by
/// earlier CloudNode versions. Kept byte-for-byte identical to the old
/// implementation so existing ciphertexts still decrypt.
fn derive_key_legacy() -> [u8; 32] {
    let host = sysinfo::System::host_name()
        .unwrap_or_else(|| "opensentry-fallback".to_string());
    let mut hasher = Sha256::new();
    hasher.update(host.as_bytes());
    hasher.update(KEY_DOMAIN_V1_LEGACY);
    hasher.finalize().into()
}

/// Encrypt with an explicit key (pure — no machine-id dependency, testable).
fn encrypt_with_key(key: &[u8; 32], plaintext: &str) -> std::result::Result<String, String> {
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|e| format!("encrypt: {}", e))?;
    let mut combined = nonce.to_vec();
    combined.extend_from_slice(&ciphertext);
    Ok(to_hex(&combined))
}

/// Decrypt with an explicit key (pure — no machine-id dependency, testable).
fn decrypt_with_key(key: &[u8; 32], hex_str: &str) -> std::result::Result<String, String> {
    let combined = from_hex(hex_str).map_err(|e| format!("hex decode: {}", e))?;
    if combined.len() < 13 {
        return Err("ciphertext too short".into());
    }
    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "decryption failed (wrong key or corrupted data)".to_string())?;
    String::from_utf8(plaintext).map_err(|e| format!("utf8: {}", e))
}

/// Encrypt a plaintext string → hex-encoded (nonce ‖ ciphertext).
///
/// Uses the v2 (machine-id-derived) key.
fn encrypt_value(plaintext: &str) -> std::result::Result<String, String> {
    let key = derive_key()?;
    encrypt_with_key(&key, plaintext)
}

/// Magic bytes prefixing every encrypted BLOB. Chosen so it can't collide
/// with the start of a JPEG (`FFD8FF`) or an MPEG-TS packet (`0x47`), which
/// are the two plaintext shapes we store — keeping `decrypt_bytes` able to
/// cleanly reject any accidentally-unencrypted blob that ever sneaks in.
///
/// The last two bytes are the format version + key version:
///   - `\x02\x01` (V1) — original format, no AAD bound to the auth tag.
///     Read-only legacy support: `decrypt_bytes` will still accept these
///     blobs, ignoring the caller's `aad` argument.
///   - `\x02\x02` (V2) — current format. The caller's `aad` (typically
///     `(kind, camera_id, identifier)` per the helpers below) is fed into
///     AES-GCM along with the plaintext, so a swapped-row attack — pasting
///     an encrypted blob from row A into row B — fails decryption when the
///     reader recomputes the AAD against row B's identity.
///
/// Bumping the version byte (rather than keeping a single magic and letting
/// AAD-mismatch produce `WrongKeyOrCorrupted`) means existing recordings
/// keep playing back after this change ships — a fresh-install node writes
/// V2 from the first segment, but an upgrade-in-place node still reads its
/// pre-existing V1 blobs without re-encryption.
const BLOB_MAGIC_V1: &[u8; 5] = b"OSE\x02\x01";
const BLOB_MAGIC_V2: &[u8; 5] = b"OSE\x02\x02";

/// Build the AAD string for a snapshot blob. Stable per row — derived from
/// fields that never change after write — so the auth tag the writer baked
/// in still verifies for every legitimate read.
///
/// A blob written for one snapshot row decrypts under exactly that row's
/// AAD; pasting it into another row's `data` column produces a mismatch
/// and `decrypt_bytes` returns `WrongKeyOrCorrupted`.
pub(crate) fn snapshot_aad(camera_id: &str, filename: &str, timestamp: i64) -> Vec<u8> {
    format!("snapshot|{}|{}|{}", camera_id, filename, timestamp).into_bytes()
}

/// AAD for a recording-segment blob. See [`snapshot_aad`] for the rationale.
pub(crate) fn recording_aad(camera_id: &str, segment_seq: u64, date: &str) -> Vec<u8> {
    format!("recording|{}|{}|{}", camera_id, segment_seq, date).into_bytes()
}

/// Encrypt raw bytes for BLOB storage. Layout:
///     [5-byte magic][12-byte nonce][ciphertext || 16-byte GCM tag]
///
/// Takes `&[u8]` (vs. `&str` for `encrypt_value`) because JPEG and MPEG-TS
/// aren't UTF-8. Returns `Vec<u8>` so the caller can bind it straight into a
/// rusqlite BLOB parameter — no hex roundtrip, no ~2× storage bloat.
///
/// `aad` is bound to the auth tag so a downstream `decrypt_bytes` call has
/// to present the same byte string to recover the plaintext. Use the
/// `snapshot_aad` / `recording_aad` helpers above so writers and readers
/// agree on the encoding. An empty `aad` is permitted (mirrors V1
/// behaviour) but loses the swap-row defense.
pub(crate) fn encrypt_bytes(plaintext: &[u8], aad: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let key = derive_key()?;
    let cipher = Aes256Gcm::new((&key).into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad })
        .map_err(|e| format!("encrypt: {}", e))?;
    let mut out = Vec::with_capacity(BLOB_MAGIC_V2.len() + nonce.len() + ciphertext.len());
    out.extend_from_slice(BLOB_MAGIC_V2);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Reasons a `decrypt_bytes` call can fail.
///
/// Splitting these out (rather than returning a single `String`) lets a
/// caller log "this row was never encrypted" differently from "this host
/// can't read this row" differently from "this ciphertext was tampered
/// with" — three diagnoses with three different operator responses.
///
/// Cryptographically you can't tell `WrongKeyOrCorrupted` apart from a
/// single blob; AES-GCM's auth tag fails identically in both cases. The
/// distinction emerges from context (does decrypt also fail for *other*
/// blobs in the same DB? Then it's a key issue, not corruption).
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DecryptError {
    /// Input shorter than `magic + nonce + GCM tag`. Definitely not a
    /// valid encrypted blob; either truncated on disk or never written.
    BlobTooShort,
    /// First 5 bytes don't match `BLOB_MAGIC`. The row was never written
    /// through `encrypt_bytes` — likely a legacy plaintext row from
    /// before encryption shipped, or arbitrary bytes that ended up in
    /// the blob column.
    NotEncrypted,
    /// AES-GCM verification failed. Either the host's machine-id-derived
    /// key doesn't match what wrote this blob (DB cloned to a new
    /// machine?) or the ciphertext was modified after write.
    WrongKeyOrCorrupted,
    /// `derive_key()` itself failed — couldn't read the OS machine
    /// identifier, etc. Carries the original error for diagnosis.
    KeyDerivation(String),
}

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlobTooShort => write!(
                f,
                "blob too short to be encrypted (need >= {} bytes for magic + nonce + GCM tag)",
                BLOB_MAGIC_V1.len() + 12 + 16,
            ),
            Self::NotEncrypted => write!(
                f,
                "blob is not in encrypted format (missing OSE magic prefix); \
                 likely an unencrypted legacy row",
            ),
            Self::WrongKeyOrCorrupted => write!(
                f,
                "decryption failed: AES-GCM auth tag mismatch — wrong key for this host \
                 (DB cloned from a different machine?) or ciphertext was tampered with",
            ),
            Self::KeyDerivation(msg) => write!(f, "could not derive encryption key: {}", msg),
        }
    }
}

impl std::error::Error for DecryptError {}

/// Decrypt a BLOB written by `encrypt_bytes`. Refuses any input that isn't
/// prefixed with a recognized magic so a legacy unencrypted row doesn't get
/// passed into AES-GCM and come back with a confusing generic "decryption
/// failed".
///
/// `aad` is the additional authenticated data the caller used at write time.
/// For V2 blobs the auth tag covers it, so a mismatch produces
/// `WrongKeyOrCorrupted`. For V1 blobs (legacy, pre-AAD format) the
/// argument is ignored — those rows decrypt with no AAD verification, since
/// the writer never bound any.
///
/// Returns a typed `DecryptError` so the caller can log the root cause
/// rather than guessing from a string-matched message. See `DecryptError`
/// docstrings for what each variant means operationally.
#[allow(dead_code)]
pub(crate) fn decrypt_bytes(blob: &[u8], aad: &[u8]) -> std::result::Result<Vec<u8>, DecryptError> {
    if blob.len() < BLOB_MAGIC_V1.len() + 12 + 16 {
        return Err(DecryptError::BlobTooShort);
    }
    let (magic, rest) = blob.split_at(BLOB_MAGIC_V1.len());

    // Decide which AAD to use for verification:
    //   V2 — caller's aad
    //   V1 — empty bytes (the writer didn't bind any AAD to the tag, so
    //        passing the caller's aad here would produce a false negative)
    let verification_aad: &[u8] = match magic {
        m if m == BLOB_MAGIC_V2 => aad,
        m if m == BLOB_MAGIC_V1 => b"",
        _ => return Err(DecryptError::NotEncrypted),
    };

    let (nonce_bytes, ciphertext) = rest.split_at(12);
    let key = derive_key().map_err(DecryptError::KeyDerivation)?;
    let cipher = Aes256Gcm::new((&key).into());
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, Payload { msg: ciphertext, aad: verification_aad })
        .map_err(|_| DecryptError::WrongKeyOrCorrupted)
}

/// Decrypt a hex-encoded (nonce ‖ ciphertext) → plaintext string.
///
/// Tries the current (machine-id) key first. If that fails, tries the legacy
/// hostname-derived key for DBs written by older CloudNode versions. Returns
/// `(plaintext, was_legacy)` — the caller re-encrypts with the new key when
/// `was_legacy` is true so the next load is fast and the legacy path is
/// eventually exercised to zero on every install.
fn decrypt_value(hex_str: &str) -> std::result::Result<(String, bool), String> {
    let new_key = derive_key()?;
    if let Ok(pt) = decrypt_with_key(&new_key, hex_str) {
        return Ok((pt, false));
    }
    let legacy_key = derive_key_legacy();
    match decrypt_with_key(&legacy_key, hex_str) {
        Ok(pt) => Ok((pt, true)),
        Err(_) => Err("decryption failed (wrong machine or corrupted data)".into()),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(hex: &str) -> std::result::Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("odd hex length".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("bad hex at {}", i))
        })
        .collect()
}

#[cfg(test)]
mod encryption_tests {
    use super::*;

    #[test]
    fn roundtrip_with_known_key() {
        let key = [0x42u8; 32];
        let ct = encrypt_with_key(&key, "super secret value").unwrap();
        let pt = decrypt_with_key(&key, &ct).unwrap();
        assert_eq!(pt, "super secret value");
    }

    #[test]
    fn roundtrip_preserves_empty_string() {
        let key = [0x01u8; 32];
        let ct = encrypt_with_key(&key, "").unwrap();
        assert_eq!(decrypt_with_key(&key, &ct).unwrap(), "");
    }

    #[test]
    fn roundtrip_preserves_utf8() {
        let key = [0x7fu8; 32];
        let pt_in = "tokens: 🔐 ñ é 中文";
        let ct = encrypt_with_key(&key, pt_in).unwrap();
        assert_eq!(decrypt_with_key(&key, &ct).unwrap(), pt_in);
    }

    #[test]
    fn nonce_randomness_produces_distinct_ciphertexts() {
        // AEAD encrypting the same plaintext twice must use a fresh nonce,
        // or confidentiality is gone. A round-trip test won't catch this —
        // check that two encryptions of the same plaintext differ.
        let key = [0x33u8; 32];
        let a = encrypt_with_key(&key, "same plaintext").unwrap();
        let b = encrypt_with_key(&key, "same plaintext").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn decrypt_rejects_wrong_key() {
        let ct = encrypt_with_key(&[1u8; 32], "secret").unwrap();
        assert!(decrypt_with_key(&[2u8; 32], &ct).is_err());
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        // AES-GCM is authenticated — flipping a byte in the tag or body
        // must fail decrypt. Prevents silent corruption / bit-flip attacks.
        let key = [0x44u8; 32];
        let ct = encrypt_with_key(&key, "hello").unwrap();
        let mut bytes = from_hex(&ct).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let tampered = to_hex(&bytes);
        assert!(decrypt_with_key(&key, &tampered).is_err());
    }

    #[test]
    fn decrypt_rejects_truncated_ciphertext() {
        // Nonce is 12 bytes; anything shorter than nonce + 1 byte of tag is
        // rejected up front rather than being handed to AES-GCM.
        let key = [0x55u8; 32];
        assert!(decrypt_with_key(&key, "").is_err());
        assert!(decrypt_with_key(&key, "00").is_err());
        assert!(decrypt_with_key(&key, &to_hex(&[0u8; 12])).is_err());
    }

    #[test]
    fn decrypt_rejects_bad_hex() {
        let key = [0x66u8; 32];
        assert!(decrypt_with_key(&key, "not hex at all").is_err());
        assert!(decrypt_with_key(&key, "abc").is_err()); // odd length
    }

    #[test]
    fn legacy_and_new_keys_are_distinct() {
        // Even if a hostile environment somehow produced the same value for
        // the hostname and the machine-id, the v1/v2 domain tags guarantee
        // the derived keys still differ.
        let host_matches_id = "constant";
        let mut v1 = Sha256::new();
        v1.update(host_matches_id.as_bytes());
        v1.update(KEY_DOMAIN_V1_LEGACY);
        let v1_key: [u8; 32] = v1.finalize().into();

        let mut v2 = Sha256::new();
        v2.update(host_matches_id.as_bytes());
        v2.update(KEY_DOMAIN_V2);
        let v2_key: [u8; 32] = v2.finalize().into();

        assert_ne!(v1_key, v2_key);
    }

    #[test]
    fn legacy_ciphertext_cannot_decrypt_with_new_key() {
        // Simulates the migration scenario: DB written by the old code can't
        // be opened with the new key, but decrypt_with_key against the
        // legacy key still works.
        let legacy = [0xaau8; 32];
        let new = [0xbbu8; 32];
        let ct = encrypt_with_key(&legacy, "api_key_abc123").unwrap();
        assert!(decrypt_with_key(&new, &ct).is_err());
        assert_eq!(
            decrypt_with_key(&legacy, &ct).unwrap(),
            "api_key_abc123",
        );
    }

    #[test]
    fn hex_roundtrip() {
        let data = vec![0x00, 0xff, 0x42, 0xab, 0xcd];
        assert_eq!(to_hex(&data), "00ff42abcd");
        assert_eq!(from_hex("00ff42abcd").unwrap(), data);
    }

    #[test]
    fn from_hex_rejects_odd_length() {
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn from_hex_rejects_non_hex_chars() {
        assert!(from_hex("gg").is_err());
        assert!(from_hex("xy").is_err());
    }

    // ── Live platform smoke test ──────────────────────────────────────
    //
    // Runs the real `machine_id()` / `derive_key()` against the host the
    // tests run on. It's the only test that actually shells out (on
    // Windows + macOS) or reads /etc/machine-id (on Linux), so it's
    // deliberately lightweight — we just check that the lookup works and
    // the derived key is non-zero. CI and local dev all exercise it.

    #[test]
    fn machine_id_returns_nonempty_on_this_platform() {
        let id = machine_id().expect("machine_id() must succeed on this platform");
        assert!(!id.is_empty(), "machine ID came back empty");
        // Sanity: both Linux's /etc/machine-id (32 hex) and Windows/macOS
        // GUIDs (36 chars with dashes) comfortably exceed 8 characters.
        assert!(
            id.len() >= 8,
            "machine ID suspiciously short ({} chars): {:?}",
            id.len(),
            id,
        );
    }

    #[test]
    fn derive_key_produces_nonzero_output() {
        let k = derive_key().expect("derive_key() must succeed on this platform");
        assert_ne!(k, [0u8; 32], "derived key is all zeros");
    }

    #[test]
    fn derive_key_is_deterministic_within_process() {
        let a = derive_key().expect("first derive_key");
        let b = derive_key().expect("second derive_key");
        assert_eq!(a, b);
    }

    // ── BLOB encryption (binary path) ────────────────────────────
    //
    // These hit the real `derive_key()` — same assumption as the smoke
    // tests above. Keeps the end-to-end path honest: if this round-trips
    // on the host we test on, recording segments will round-trip too.

    #[test]
    fn encrypt_bytes_roundtrip() {
        // Use a representative binary payload — first 8 bytes of a JPEG
        // header plus some random-ish bytes — so we catch any UTF-8
        // assumption that might have crept in.
        let plaintext: Vec<u8> = (0..=255).chain(0..=200).collect();
        let aad = b"snapshot|cam_test|frame.jpg|1700000000";
        let ct = encrypt_bytes(&plaintext, aad).expect("encrypt_bytes");
        let pt = decrypt_bytes(&ct, aad).expect("decrypt_bytes");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn encrypt_bytes_prepends_v2_magic_prefix() {
        let ct = encrypt_bytes(b"hello", b"aad").expect("encrypt_bytes");
        assert!(
            ct.starts_with(BLOB_MAGIC_V2),
            "ciphertext did not start with V2 magic prefix: {:?}",
            &ct[..BLOB_MAGIC_V2.len().min(ct.len())],
        );
    }

    #[test]
    fn encrypt_bytes_is_nondeterministic() {
        // Re-encrypting the same plaintext must produce distinct ciphertexts
        // (fresh nonce per call). Without this, AES-GCM confidentiality is
        // gone — an attacker can tell when two segments match.
        let a = encrypt_bytes(b"same plaintext", b"aad").expect("encrypt a");
        let b = encrypt_bytes(b"same plaintext", b"aad").expect("encrypt b");
        assert_ne!(a, b);
    }

    #[test]
    fn decrypt_bytes_rejects_aad_mismatch() {
        // The whole point of the AAD plumbing: a blob that was encrypted
        // for snapshot row A must NOT decrypt when the reader thinks it's
        // for row B. That's the swap-row defence — fail closed, surface as
        // WrongKeyOrCorrupted.
        let plaintext = b"row A's secret";
        let ct = encrypt_bytes(
            plaintext,
            &snapshot_aad("cam_a", "frame.jpg", 1_700_000_000),
        )
        .expect("encrypt");
        // Same camera, same filename, *different* timestamp — the kind of
        // forgery a swap-row attacker would attempt.
        let err = decrypt_bytes(
            &ct,
            &snapshot_aad("cam_a", "frame.jpg", 1_700_000_001),
        )
        .expect_err("AAD mismatch must fail");
        assert_eq!(err, DecryptError::WrongKeyOrCorrupted);
    }

    #[test]
    fn decrypt_bytes_reads_v1_legacy_blobs_with_any_aad() {
        // Pre-v0.1.18 nodes wrote V1 blobs (no AAD bound). Existing
        // recordings on an upgraded node must keep playing back, so V1
        // decrypt ignores the caller's AAD argument.
        //
        // We can't load a real v0.1.16 blob in a unit test, but we can
        // hand-build one: encrypt with empty AAD under the V2 path, then
        // patch the magic to V1. That's byte-identical to what the old
        // code would have produced.
        let mut ct = encrypt_bytes(b"legacy payload", b"").expect("encrypt");
        ct[..BLOB_MAGIC_V1.len()].copy_from_slice(BLOB_MAGIC_V1);

        // Caller passes any AAD it wants — V1 path ignores it.
        let pt = decrypt_bytes(&ct, b"this gets ignored").expect("V1 decrypt");
        assert_eq!(pt, b"legacy payload");
    }

    #[test]
    fn decrypt_bytes_rejects_unencrypted_blob() {
        // Raw JPEG-like bytes without the magic prefix must be refused
        // cleanly — not handed to AES-GCM, which would only surface a
        // generic tag-failure and obscure the real "this blob was never
        // encrypted" diagnosis.
        let raw: [u8; 40] = [0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                             0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                             0, 0, 0, 0, 0, 0, 0, 0];
        let err = decrypt_bytes(&raw, b"").expect_err("must refuse missing magic");
        assert_eq!(err, DecryptError::NotEncrypted);
    }

    #[test]
    fn decrypt_bytes_rejects_short_blob() {
        // Anything shorter than magic + nonce + minimum tag bytes is
        // rejected up front rather than handed to AES-GCM.
        assert_eq!(decrypt_bytes(&[], b"").unwrap_err(), DecryptError::BlobTooShort);
        assert_eq!(
            decrypt_bytes(b"OSE\x02\x01", b"").unwrap_err(),
            DecryptError::BlobTooShort,
        );
        assert_eq!(
            decrypt_bytes(&[0u8; 16], b"").unwrap_err(),
            DecryptError::BlobTooShort,
        );
    }

    #[test]
    fn decrypt_bytes_rejects_tampered_ciphertext() {
        // Flip a byte in the tag region (last 16 bytes) — AES-GCM is
        // authenticated, so this must fail cleanly rather than return
        // garbled plaintext. From a single blob we can't tell tamper
        // apart from "wrong key for this host", so both surface as
        // ``WrongKeyOrCorrupted`` — the operator triages by checking
        // whether *other* blobs from the same DB also fail.
        let aad = b"recording|cam_x|42|2026-04-25";
        let mut ct = encrypt_bytes(b"hello world", aad).expect("encrypt");
        let last = ct.len() - 1;
        ct[last] ^= 0xff;
        assert_eq!(
            decrypt_bytes(&ct, aad).unwrap_err(),
            DecryptError::WrongKeyOrCorrupted,
        );
    }

    #[test]
    fn aad_helpers_format_deterministically() {
        // The helpers must produce byte-identical AAD given the same inputs
        // — otherwise reads would silently fail to verify against writes.
        // Pin the format so a future "let's just rearrange the fields"
        // refactor lights this up before it ships and silently breaks the
        // read path.
        assert_eq!(
            snapshot_aad("cam_garage", "10-30-15.jpg", 1_700_000_000),
            b"snapshot|cam_garage|10-30-15.jpg|1700000000".to_vec(),
        );
        assert_eq!(
            recording_aad("cam_garage", 42, "2026-04-25"),
            b"recording|cam_garage|42|2026-04-25".to_vec(),
        );
    }

    #[test]
    fn decrypt_error_messages_distinguish_root_causes() {
        // The Display impl is what ends up in operator logs, so it must
        // make each variant grep-able to its diagnosis.
        assert!(DecryptError::NotEncrypted.to_string().contains("magic"));
        assert!(DecryptError::BlobTooShort.to_string().contains("too short"));
        assert!(
            DecryptError::WrongKeyOrCorrupted
                .to_string()
                .contains("auth tag mismatch"),
        );
        assert!(
            DecryptError::KeyDerivation("permission denied".into())
                .to_string()
                .contains("permission denied"),
        );
    }
}

#[cfg(test)]
mod retention_reclaim_tests {
    use super::*;

    /// Sum the on-disk footprint of a SQLite DB across its main file plus
    /// the WAL / SHM sidecars.  We compare this before vs after retention
    /// because freshly-written data may sit in the `-wal` sidecar until a
    /// checkpoint folds it into the main file.
    fn db_footprint_bytes(db_path: &std::path::Path) -> u64 {
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let p = if suffix.is_empty() {
                db_path.to_path_buf()
            } else {
                let mut s = db_path.as_os_str().to_os_string();
                s.push(suffix);
                std::path::PathBuf::from(s)
            };
            if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
        total
    }

    #[test]
    fn enforce_retention_actually_shrinks_the_db_file() {
        // Regression guard for the silent Pi disk-fill: before the
        // auto_vacuum=INCREMENTAL + incremental_vacuum + wal-truncate
        // fix, enforce_retention's DELETEs freed pages to the freelist
        // but the .db file never shrank — the cap accounting
        // (SUM(size_bytes)) read low while the actual file stayed at its
        // high-water mark.  This writes ~2 MB, enforces a tiny cap, and
        // asserts the real on-disk footprint drops.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("node.db");
        let db = NodeDatabase::new(&db_path).expect("db opens");

        // 20 × 100 KB recording segments ≈ 2 MB of payload.
        let blob = vec![0xABu8; 100 * 1024];
        for seq in 0..20u64 {
            db.save_recording_segment("cam_reclaim", seq, "2026-05-28", &blob, 1000)
                .expect("save segment");
        }

        let footprint_before = db_footprint_bytes(&db_path);
        assert!(
            footprint_before > 1_500_000,
            "sanity: ~2 MB of segments should be on disk, got {} bytes",
            footprint_before,
        );

        // Cap at 64 KB — forces deleting all but (at most) the last
        // segment, then reclaiming.
        let (current, freed) = db.enforce_retention(64 * 1024).expect("retention runs");
        assert!(freed > 1_000_000, "should free ~the whole payload, freed {}", freed);
        // Row-total accounting is at/under the cap (plus at most one
        // not-yet-deleted segment that already pushed us under `excess`).
        assert!(
            current <= 64 * 1024 + 100 * 1024,
            "row total should be near the cap, got {}",
            current,
        );

        let footprint_after = db_footprint_bytes(&db_path);
        assert!(
            footprint_after < footprint_before,
            "on-disk footprint must shrink after retention reclamation — \
             before={} after={} (auto_vacuum/incremental_vacuum regressed)",
            footprint_before,
            footprint_after,
        );
    }

    #[test]
    fn fresh_db_is_incremental_auto_vacuum() {
        // The pragma must take effect on a brand-new DB with no VACUUM —
        // i.e. it's set before CREATE TABLE.  If a refactor moves the
        // pragma after table creation, a fresh DB silently falls back to
        // auto_vacuum=NONE and reclamation stops working for new installs.
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");
        let conn = db.lock().expect("lock");
        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .expect("query auto_vacuum");
        assert_eq!(mode, 2, "fresh DB must be INCREMENTAL (2), got {}", mode);
    }
}

#[cfg(test)]
mod recording_segment_listing_tests {
    use super::*;

    /// `list_recording_segment_seqs` feeds the VOD M3U8 builder, which
    /// emits one `segment_<seq>.ts` line per row it returns.  Because the
    /// table has no UNIQUE(camera_id, date, segment_seq) constraint and
    /// FFmpeg's `segment_%05d.ts` counter restarts at 0 on every pipeline
    /// restart, two distinct rows can share the same (camera, date, seq)
    /// within one UTC day.  Without de-dup the playlist would list that
    /// seq twice — the player replays the same ~1s and the colliding blob
    /// is unreachable.  Pin that the listing collapses dupes to one row
    /// per seq, keeping the longer duration.
    #[test]
    fn list_recording_segment_seqs_dedups_colliding_seqs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");

        let date = "2026-05-28";
        // Monotonic first run: seqs 1,2,3.
        for seq in 1..=3u64 {
            db.save_recording_segment("cam_x", seq, date, b"first-run", 1000)
                .expect("save segment");
        }
        // FFmpeg restarted mid-day and reused seqs 1 and 2 with different
        // footage and a longer (max) duration.
        db.save_recording_segment("cam_x", 1, date, b"second-run-1", 2000)
            .expect("save dup seq 1");
        db.save_recording_segment("cam_x", 2, date, b"second-run-2", 2000)
            .expect("save dup seq 2");

        let rows = db
            .list_recording_segment_seqs("cam_x", date)
            .expect("list seqs");

        // Five physical rows, but exactly one entry per distinct seq.
        let seqs: Vec<u64> = rows.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 3],
            "duplicate seqs must collapse to a single playlist entry each",
        );
        // MAX(duration_ms) survives for a collided seq; untouched seq keeps its own.
        let dur_for = |want: u64| rows.iter().find(|(s, _)| *s == want).map(|(_, d)| *d);
        assert_eq!(
            dur_for(1),
            Some(2000),
            "collided seq keeps the longer duration"
        );
        assert_eq!(dur_for(3), Some(1000));
    }

    /// The dedup guard must be a no-op on the happy path: a clean run with
    /// no collisions is returned verbatim, ascending.
    #[test]
    fn list_recording_segment_seqs_preserves_unique_runs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");

        let date = "2026-05-28";
        for seq in [3u64, 1, 2, 7] {
            db.save_recording_segment("cam_y", seq, date, b"x", 1000)
                .expect("save segment");
        }
        let rows = db
            .list_recording_segment_seqs("cam_y", date)
            .expect("list seqs");
        let seqs: Vec<u64> = rows.iter().map(|(s, _)| *s).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 3, 7],
            "unique seqs returned ascending, unchanged"
        );
    }
}
