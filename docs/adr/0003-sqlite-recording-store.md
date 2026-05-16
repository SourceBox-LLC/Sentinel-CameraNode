# ADR 0003: SQLite for recording + snapshot storage

- **Status:** Accepted
- **Date:** 2026-04
- **Deciders:** CloudNode maintainers

## Context

CloudNode needs to store two kinds of binary data locally on the customer's hardware:

1. **Recordings** — MPEG-TS segments produced by FFmpeg, written continuously while recording is active. Multi-megabyte each, several thousand per camera per day.
2. **Snapshots** — JPEG frames captured on demand or in response to motion events. ~50–200 KB each.

Both need to be:
- **Encrypted at rest** — see ADR 0002. Plain JPEGs sitting in a directory don't meet the customer-promise on `/security`.
- **Retention-managed** — oldest items evicted when the user-configured `storage.max_size_gb` quota is hit.
- **Queryable by metadata** — "recordings for camera X between time A and B" needs to be O(log n), not a full directory walk.
- **Backup-friendly** — a single thing to copy, ideally one file the operator can `cp` off-device.
- **Durable across crashes** — partial writes that get interrupted by a power loss must not corrupt anything else.

We considered four storage strategies:

1. **Flat files in nested directories.** `data/recordings/{camera_id}/{date}/{seq}.ts` etc. Simple, every tool understands files. Cost: encryption-at-rest means writing custom file-level encryption (no good off-the-shelf format that's both Rust-native and stable); retention requires a directory walk; backups are "tar the whole tree"; partial writes leave half-files that the next sweep has to detect and clean up.
2. **Embedded KV store** (`sled`, `rocksdb`). Better than flat files for blob-with-metadata workloads. Cost: `rocksdb` adds a 30+ MB compiled-in dependency, `sled` is unmaintained as of 2024, both have less mature Rust bindings than rusqlite, and neither comes with SQL — which we'd want for the retention sweeps anyway.
3. **SQLite with BLOB columns.** Boring, well-understood, single-file, transaction-safe, query-by-SQL.
4. **External object storage** (Tigris / S3 / minio). Right answer if "the customer doesn't own the storage" was the goal. Wrong answer for CloudNode, where the whole pitch is "your recordings stay on your hardware." (Command Center *did* try Tigris for the live-streaming hot path and then deleted it — see Command Center's git history around the in-memory cache rewrite.)

## Decision

**Recordings and snapshots live in `data/node.db` (SQLite) as encrypted BLOB columns alongside their metadata rows.** Same DB as the configuration table; one file holds everything.

Tables (defined inline in `src/storage/database.rs::NodeDatabase::initialize_schema`):

- `recording_segments(id, camera_id, segment_seq, date, data, size_bytes, duration_ms)` — one row per `.ts` segment as it lands from FFmpeg.  `date` is `YYYY-MM-DD` (UTC bucket); `data` is the AES-256-GCM ciphertext; `size_bytes` is the *plaintext* length (so retention accounting matches what the operator sees in the disk-usage UI).  `duration_ms` lets the dynamic VOD playlist served by the local web UI emit accurate per-segment `#EXTINF` values.  Index `idx_rec_camera_date` on `(camera_id, date)` makes "all segments for this camera on this day" an O(log n) lookup.
- `snapshots(id, camera_id, filename, timestamp, data, size_bytes)` — one row per JPEG. `filename` is a stable human-readable identifier built from camera_id + ISO timestamp.  Index `idx_snap_camera` on `(camera_id)`.
- `config(key, value)` — KV store for setup config (including the AES-encrypted `api_key`).
- `logs(id, timestamp, level, message)` — tracing-layer events, survives restarts so the TUI shows prior history.  Index `idx_logs_timestamp` on `(id DESC)` for the most-recent-N reads.
- `local_recording_state(camera_id, enabled)` — Local-mode-only persistence for the per-camera recording toggle (Connected mode treats it as transient — heartbeat reconciler overrides on each tick).

There is **no** session-level `recordings` table.  Recording is segment-only: every TS chunk FFmpeg writes is an independent row.  The dashboard groups them at query time by `(camera_id, date)` to produce a "recording bucket" view; the dynamic VOD playlist concatenates them at playback time.  Considering a session-level rollup was rejected because (a) the operator can already delete by `(camera_id, date)`, (b) retention math is cleaner against rows of bounded size, and (c) the live + on-demand recording paths converge on the same data model.

Encryption uses the AES-256-GCM `encrypt_bytes` path described in ADR 0002 — same machine-id-derived key as for the API key.  Current writes use the V2 wire format with magic prefix `OSE\x02\x02` (`BLOB_MAGIC_V2`); the V1 prefix `OSE\x02\x01` is still read-supported so nodes upgraded from pre-v0.1.18 can decrypt their existing blobs without re-encryption.  AAD (additional authenticated data) is a pipe-delimited string built from the row's identity tuple — `"snapshot\|{camera_id}\|{filename}\|{timestamp}"` for snapshots, `"recording\|{camera_id}\|{segment_seq}\|{date}"` for recordings — so a swap-row attack at rest fails decryption.

Retention is a SQL query.  `enforce_retention(max_bytes)` walks oldest-first by `id` (which is monotonic per insert, so it sorts by insertion order which is effectively by time):

```sql
SELECT id, size_bytes FROM recording_segments ORDER BY id ASC;
-- iterate, summing `size_bytes`, until cumulative freed_bytes >= excess;
-- for each id we keep, run:
DELETE FROM recording_segments WHERE id = ?1;
```

Same shape for `snapshots`.  SQLite handles the I/O.  Retention runs on the 5-minute storage-stats tick; the safety-floor disk-low check pauses *new* writes earlier (at 1 GiB free) so reclaiming via this path always has somewhere to write the freed-up space.

Backups: `cp data/node.db data/node.db.backup` while the daemon is paused (or use the SQLite Online Backup API, which we don't currently expose but could trivially add). One file out, one file in.

## Consequences

**Positive:**
- **One file is the whole state.** Operator backups are trivial. Restore is `cp` and restart.
- **Transactions.** A power loss mid-write loses at most the in-flight transaction; the DB itself stays consistent. SQLite's WAL mode is well-tested. We don't have to write any of the "did the file finish writing? scan-and-recover" logic ourselves.
- **Retention is SQL.** Oldest-first iteration via `SELECT id, size_bytes ... ORDER BY id ASC` (id is monotonic, ~= insert time) runs in milliseconds even on multi-GB DBs.  A flat-file equivalent is a directory walk + sort + unlink loop that's both slower and more code.
- **Encryption rides on existing infrastructure.** Same `encrypt_bytes` / `decrypt_bytes` pair used for API keys. One crypto path, one set of tests, one place to upgrade if/when we move beyond AES-256-GCM.
- **Same DB as config.** No multi-store transaction problems. Recording metadata and node config update in the same transaction when needed.
- **Cross-platform.** SQLite ships everywhere. `rusqlite` works identically on Linux / macOS / Windows / Pi.

**Negative:**
- **SQLite write amplification on multi-MB blobs.** Each segment write is a full row write — SQLite doesn't append to a BLOB in place. For a 1080p30 stream chunking at 1-second HLS segments, that's one row INSERT per second per camera. We measured ~5% CPU on Pi 4 for two cameras at this rate; acceptable, but worth knowing if the design ever has to scale to dozens of streams per node.
- **The 281 TB SQLite per-file limit is theoretical; the realistic ceiling is closer to a few hundred GB** before queries start to feel sluggish on cheap NVMe and much worse on SD cards. We document `storage.max_size_gb` defaults at 64 GB. A customer who configures 200 GB will hit no SQLite-imposed problem but should expect retention sweeps to take noticeable seconds.
- **No streaming reads.** When the dashboard wants to play back a recording, the entire blob is decrypted into memory before being streamed to the response. For a 5-minute 1080p recording at 2 Mbps that's ~75 MB in RAM transiently. On a Pi 4 with 4 GB this is a non-issue; on a Pi Zero 2W with 512 MB this is the ceiling that decides how long a recording can be before playback OOMs. We document the Pi Zero 2W as single-camera-only for this reason.
- **Encryption + SQLite locking** means every recording write is fsync'd. Disk-IOPS-limited Pi setups (cheap SD cards) can stall the encoder if the writer falls behind. The supervisor's stall watchdog catches this and restarts FFmpeg, but the "your SD card is too slow" failure mode is real and surfaces as motion events going through but recordings dropping. We surface this in `docs/runbooks/video-not-showing.md`.

**Neutral:**
- The schema is segment-only — there is no session-level `recordings` table.  Considered and rejected; rationale lives in the "Decision" section above.
- SQLite's `PRAGMA journal_mode=WAL` is the default; we rely on it for crash safety and concurrent-read-during-write.

## Revisiting this decision

Re-evaluate if any of the following become true:

- A customer routinely runs into the "DB feels slow at 200 GB" boundary. Two responses available: (a) move blobs to a sidecar file-per-row format, keeping metadata in SQLite — common pattern for "metadata in SQL, blobs on disk"; (b) split node.db into separate config + recording databases, keeping the same encryption.
- We add a streaming-decryption format (so playback doesn't have to load the full blob into memory). Probably file-per-segment with an encrypted-stream wrapper. Pi Zero 2W length ceiling goes away.
- A new platform ships where SQLite isn't the obvious choice (e.g. a heavy ARM-server CloudNode running 50+ cameras on enterprise NVMe — the per-row INSERT overhead starts to be a real loss vs. a streaming append-only file format).
- Customers want **encrypted offsite backups** — at which point a shippable backup format that's not "give us your machine-id-derived key" becomes a feature. We'd add a passphrase-wrapped key export, see ADR 0002's revisit conditions.

## References

- `src/storage/database.rs` — schema (inline in `initialize_schema`), encryption helpers (`encrypt_bytes` / `decrypt_bytes`), AAD builders (`snapshot_aad` / `recording_aad`), retention queries (`enforce_retention`), and every read/write accessor (`save_snapshot`, `save_recording_segment`, `list_recordings`, `list_recording_segment_seqs`, `get_recording_segment`, …).  All in one file by design — schema and the only code that touches it stay co-located.
- `src/server/api.rs` — the local HTTP endpoints (`/api/recordings/{cam}/{date}/playlist.m3u8`, `/api/recordings/{cam}/{date}/segment_{n}.ts`, `/api/snapshots`, `/api/snapshots/{id}`) that serve recordings + snapshots back to the SPA.
- `docs/adr/0002-machine-id-encryption-key.md` — why the encryption key is what it is.
- `docs/runbooks/video-not-showing.md` — operator runbook for the failure modes called out in the negative consequences.
- SQLite documentation on the `BLOB` storage class and the `PRAGMA journal_mode=WAL` default.
