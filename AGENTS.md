# AGENTS.md

SourceBox Sentry CloudNode — turns USB webcams into cloud-connected security cameras. Rust binary that transcodes camera video into HLS and pushes each segment directly into the Command Center's in-memory cache. **No Tigris, no S3, no presigned URLs.**

**Companion docs:**
- [`README.md`](README.md) — user-facing install and operation guide.
- [`docs/runbooks/`](docs/runbooks/) — runbooks for common failure modes (start with `video-not-showing.md`).
- [`docs/adr/`](docs/adr/) — architecture decision records (e.g. `0001-pi-software-encoding.md` for why Pi is libx264-only).

## Build & Test Commands

```bash
cargo build              # Debug build
cargo build --release    # Optimized (production)
cargo test               # Unit + integration tests
cargo clippy             # Lint
cargo fmt -- --check     # Format check
cargo run                # Development mode (falls through to setup wizard if unconfigured)
cargo run -- setup       # Force-run the setup wizard
```

### Building the local web UI (Phase C)

The browser dashboard lives in `web/` (Vite + React + TypeScript) and
gets compiled into a `web-dist/` bundle that the Rust binary embeds at
compile time via `rust-embed`.  Two-step build:

```bash
cd web
npm install              # First time only
npm run build            # Outputs to ../web-dist/
cd ..
cargo build --release    # rust-embed picks up web-dist/ automatically
```

`build.rs` writes a placeholder `web-dist/index.html` if you skip the
npm step, so `cargo build` still succeeds — but the binary serves a
"Web UI not built" page until you run the real build.  `web-dist/` is
gitignored; commit only the `web/` source.

## Operating modes

Every install picks one of two modes at the setup wizard's first
prompt — `mode` is then persisted as a SQLite KV row (`config.mode`,
values `local` / `connected`).  Default = Connected for back-compat:
existing `node.db` files written by pre-mode-flag binaries have no
`mode` row and load as Connected, so binary swap on a running node is
a no-op behaviorally.

- **Connected** — node registers with Command Center, opens the
  inbound WebSocket, sends heartbeats, pushes HLS segments.  Same
  shape as the pre-mode-flag product.
- **Local** — node skips registration, the heartbeat loop, the WS
  client, and the segment-push HTTP call.  FFmpeg supervisor + HLS
  generation + local /api/* + browser dashboard all run unchanged.
  Camera IDs are namespaced under a stable `local_node_id` UUID
  generated on first boot (also a `config` SQLite KV row), so
  per-camera dirs and DB rows survive reboots.

Runtime fork point: `node::runner::run_internal` — every CC-coupled
spawn is gated on `self.config.mode.is_connected()`.  See `NodeMode`
in `src/config/settings.rs` for the type and its `as_str` /
`from_str` helpers.

## Configuration

Config is stored in a **SQLite database** (`data/node.db`). The API key is encrypted at rest using AES-256-GCM with a machine-derived key — SHA-256 of the OS machine identifier (`/etc/machine-id` on Linux, `MachineGuid` registry key on Windows, `IOPlatformUUID` on macOS) + an application salt. The DB is not portable between machines. DBs written by older CloudNode versions (hostname-derived key) are transparently migrated to the machine-ID-derived key on first decrypt.

Loading priority (in `Config::load()`):

1. **SQLite database** (`data/node.db`) — primary, created by setup wizard
2. **YAML file** (`config.yaml`) — legacy fallback, auto-migrated to DB on first load
3. **Environment variables** — override any DB/YAML values:
   - `SOURCEBOX_SENTRY_NODE_ID`, `SOURCEBOX_SENTRY_API_KEY`, `SOURCEBOX_SENTRY_API_URL`
   - `SOURCEBOX_SENTRY_ENCODER` — video encoder override (e.g. `h264_nvenc`)
   - `RUST_LOG` — log level
4. **CLI flags** — highest priority: `--node-id`, `--api-key`, `--api-url`

### Config sections (`Config` in `src/config/settings.rs`)

- `node` — friendly name
- `cloud` — `api_url`, `api_key` (never serialised), `heartbeat_interval`
- `cameras` — `auto_detect`, optional manual `devices` list
- `streaming` — `fps`, `jpeg_quality`, `encoder`, nested `hls` (`enabled`, `segment_duration`, `playlist_size`, `bitrate`)
- `recording` — `enabled`, `format` (`mp4` or `mkv`).  Per-camera recording policy (continuous_24_7 / scheduled_recording / scheduled_start / scheduled_end) lives backend-side on the Camera row and is reconciled to CloudNode via the heartbeat response — see "Recording flow" below.
- `storage` — `max_size_gb` (operator-chosen during setup based on disk-aware suggestion).  The legacy `path` field was removed in v0.1.40; `paths::data_dir()` is the canonical resolver.
- `server` — local HTTP `port` + `bind`
- `logging` — `level`
- `motion` — `enabled`, `threshold` (scene-change score 0.0–1.0), `cooldown_secs`

## Project Structure

```
src/
├── main.rs             # CLI entry point (clap)
├── lib.rs              # Library re-exports
├── dashboard/          # Live TUI dashboard + slash commands (split from
│   │                   # 1,761-line dashboard.rs in commit 654e88e)
│   ├── mod.rs          # module routing + pub re-exports of Dashboard, types
│   ├── types.rs        # LogLevel, LogEntry, CameraState, CameraStatus, View, SettingsInfo
│   ├── state.rs        # DashboardState struct + state-mutation methods
│   ├── handle.rs       # Dashboard wrapper struct + lifecycle/setup methods
│   │                   # (new, log_*, set_db, set_disabled_cameras, etc.)
│   ├── render.rs       # Dashboard::render + format helpers (panel rows,
│   │                   # plan badge, ANSI-aware truncation, box-drawing)
│   └── commands.rs     # run_render_loop + execute_command + confirm flow + tests
├── error.rs            # Custom Error enum + Result alias
├── logging.rs          # tracing subscriber setup
├── api/                # Cloud API client + WebSocket
│   ├── client.rs       # ApiClient — register, heartbeat, codec, push_segment, playlist, motion.
│   │                   # Has `local_stub()` constructor + `is_local()` predicate so Local-mode
│   │                   # nodes hold a no-op client without `Option<ApiClient>` plumbing.
│   ├── commands.rs     # Shared command implementations (Phase B). `take_snapshot` lives here so
│   │                   # both the WS dispatcher (Connected) and the local /api/cameras/{id}/snapshot
│   │                   # HTTP route call the same FFmpeg-grab + DB-save flow.
│   ├── websocket.rs    # WS loop with auto-reconnect; relays motion events + handles commands.
│   │                   # `cmd_take_snapshot` is now a thin adapter over `commands::take_snapshot`.
│   ├── types.rs        # Request/response types
│   └── mod.rs
├── camera/             # Detection & capture
│   ├── detector.rs     # Auto-detect USB cameras
│   ├── capture.rs      # Frame capture helpers
│   ├── platform/       # Linux (v4l2) / Windows (DirectShow) / macOS (AVFoundation)
│   ├── types.rs
│   └── mod.rs
├── config/             # Configuration
│   ├── mod.rs          # Config loader (DB → YAML → env → CLI)
│   └── settings.rs     # Config structs (see sections above)
├── node/               # Orchestrator
│   ├── runner.rs       # Node lifecycle (register, spawn pipelines, dashboard loop)
│   └── mod.rs
├── server/             # Local HTTP server (warp)
│   ├── http.rs         # Endpoints: /health, /hls/*, /api/* — binds to 127.0.0.1 (Connected) or
│   │                   # 0.0.0.0 (Local) per the wizard's bind choice.  Two constructors:
│   │                   # `new_with_hls` (legacy, no /api/*) and `new_with_api` (Phase B+).
│   ├── api.rs          # Phase B + C: `LocalApiState` + the /api/* route filter chain
│   │                   # (cameras, snapshots, recordings, status, recording-toggle) + the
│   │                   # static_routes() filter that serves the embedded SPA from web-dist/.
│   │                   # All route handlers return a uniform `ApiReply` =
│   │                   # `warp::http::Response<Vec<u8>>` so warp's `or().unify()` chain works
│   │                   # without `Box<dyn Reply>` runtime erasure.
│   └── mod.rs
├── setup/              # Interactive TUI setup wizard (crossterm + inquire).
│   │                   # First prompt asks "Connect to Command Center?" and forks the rest of
│   │                   # the wizard on mode = Local | Connected.  See `configure_node` in tui.rs.
│   ├── mod.rs          # Setup flow
│   ├── platform.rs     # Platform detection (Linux / Windows / macOS / Pi / WSL)
│   ├── tui.rs          # Terminal UI
│   ├── ui.rs           # Rendering helpers
│   ├── animations.rs   # Progress animations
│   ├── validator.rs    # Credential validation via POST /api/nodes/validate
│   ├── recovery.rs     # Error recovery and user guidance
│   └── wsl_preflight.rs # WSL2 Scope A preflight: detect WSL, pick a usable distro,
│                        #   install FFmpeg in-distro, print usbipd bind/attach
│                        #   commands for each detected USB camera. Actions that
│                        #   need admin elevation are printed, not executed.
├── streaming/          # HLS pipeline
│   ├── hls_generator.rs    # FFmpeg subprocess per camera (HLS muxer)
│   ├── supervisor.rs       # Per-camera FFmpeg supervisor: exponential-backoff restart,
│   │                       #   stall-flag watchdog, propagates CameraStatus to the dashboard
│   ├── hls_uploader.rs     # Watches HLS dir, drives playlist updates + motion event channel
│   ├── segment_uploader.rs # Posts each .ts to POST /push-segment with retry/backoff
│   ├── motion_detector.rs  # Parallel FFmpeg scene-change scorer
│   ├── codec_detector.rs   # FFprobe-based codec detection
│   └── mod.rs              # Re-exports + shared find_ffmpeg() helper
└── storage/            # SQLite-backed local storage
    ├── database.rs     # NodeDatabase: snapshots, recordings, config (all BLOB/KV).
    │                   # Phase B accessors (Local web UI): get_snapshot_data,
    │                   # list_recording_segment_seqs, get_recording_segment,
    │                   # set_local_recording, get_local_recording_state.
    │                   # `recording_segments` schema gained a `duration_ms` column
    │                   # for the dynamic VOD playlist; ALTER-on-startup migration.
    └── mod.rs

examples/
└── wsl_preflight_probe.rs  # Manual probe that runs the WSL2 preflight against
                            #   the real host and prints distros / ffmpeg / usbipd state.
                            #   Run with: cargo run --example wsl_preflight_probe

web/                        # Phase C local browser dashboard — Vite + React 18 + TypeScript.
├── package.json            #   Pinned: react 18.3, vite 5.4, hls.js 1.5, react-router-dom 6.27.
├── vite.config.ts          #   Outputs to ../web-dist/ for rust-embed pickup.  Dev proxy
│                           #   forwards /api + /hls to localhost:8080 for `npm run dev`.
├── tsconfig.json
├── index.html
└── src/
    ├── main.tsx            # React entry + react-router-dom routes
    ├── App.tsx             # Shell — brand + nav + mode pill, polls /api/status
    ├── styles.css          # Plain CSS; CSS variables match Command Center dark theme
    ├── components/
    │   └── HlsPlayer.tsx   # HLS.js wrapper + native-Safari fallback
    ├── lib/
    │   ├── api.ts          # Typed fetch wrappers per /api/* endpoint
    │   └── toasts.tsx      # Tiny dependency-free toast context
    └── pages/
        ├── CamerasPage.tsx     # Live HLS grid, snapshot + record-toggle buttons
        └── RecordingsPage.tsx  # (camera × date) cells → modal HLS player

build.rs                    # Pre-build hook — writes a placeholder web-dist/index.html if
                            #   the dir is empty so `cargo build` doesn't fail before someone
                            #   has run `npm run build`.  The placeholder satisfies the
                            #   smoke test in src/server/api.rs.
```

## Architecture

### Lifecycle

`main.rs` → `Node::new()` → `Node::run()`

**Node::run()** workflow:
1. Create live TUI dashboard (raw mode, crossterm events)
2. Detect cameras (`camera::detect_cameras()`)
3. Register with Command Center (`api_client.register()`)
4. Detect hardware encoder once (NVENC/QSV/AMF on x86; **libx264 forced on Raspberry Pi**), persist to DB
5. Coerce any retired encoders stored in DB (`RETIRED_ENCODERS` — see "Encoder coercion" below) back to auto-detect
6. Spawn one `FFmpegSupervisor` per camera (wraps `HlsGenerator` — see "Supervisor" below)
7. Spawn HLS uploader tasks (segment push + playlist update + codec detection)
8. Spawn motion detector per camera (second FFmpeg probe for scene-change scoring)
9. Launch local HTTP server (port 8080) + WebSocket client
10. Start retention task (enforces `max_size_gb` via DB)
11. Run dashboard render loop (blocks until `/quit` or Ctrl+C)

### FFmpeg supervisor (`streaming/supervisor.rs`)

Each camera's HLS pipeline runs under an `FFmpegSupervisor` rather than being spawned once and forgotten. The supervisor:

- Polls the FFmpeg child every 2s (`POLL_INTERVAL`).
- On exit, restarts FFmpeg with exponential backoff (1s → 2s → 4s → … capped at 30s, matching the WebSocket reconnect ceiling).
- Gives up and marks the camera `Failed` if it restarts more than 5 times inside a 60s window.
- Resets backoff after 60s of healthy streaming (`HEALTHY_RESET_THRESHOLD`).
- Watches a shared `stall_flag: Arc<AtomicBool>` that the uploader raises after ~20s of no new segments — a wedged-but-alive FFmpeg (V4L2 deadlock, thermal throttle below real-time, USB bandwidth starvation) gets killed and routed through the normal restart path.
- Pushes `CameraStatus::Streaming / Restarting / Failed` into the dashboard so WebSocket and HTTP heartbeats report real pipeline state instead of the old hardcoded `"streaming"`.
- Supports a `PipelineSource::TestPattern(w, h, fps)` fallback used in dev / CI when a real webcam isn't available.

Before this supervisor existed, an FFmpeg crash (disk-full, closed V4L2 fd, segment-writer failure) silently left the camera offline from the browser's point of view while the node still reported `streaming` in every heartbeat — backend MCP tools ended up telling users to "update CloudNode" when the real failure was upstream.

**Disk-exhausted annotation.** On Linux the supervisor calls `libc::statvfs` on the HLS output dir before every start and after every crash. If the filesystem is under 256 MiB free, the error string surfaced to `CameraStatus::Restarting` / `CameraStatus::Failed` is prefixed with `(disk exhausted: N MiB free)`. That string flows into heartbeats and the `get_node` MCP tool, so an operator never has to SSH in to diagnose ENOSPC — they see it directly in the dashboard. Only implemented on Linux because the Pi is where the failure mode lives; on other platforms the helper returns `None` and the error string passes through untouched.

**Orphan segment sweeper** (`streaming/hls_uploader.rs` → `sweep_orphan_segments`). Sole owner of `.ts` cleanup since v0.1.17, when `-hls_flags delete_segments` was dropped — FFmpeg's own rotation-delete raced Windows Defender / NTFS lazy-close / external readers and fired `failed to delete old segment ...` on every rotation. Every ~60s the uploader lists `data/hls/{cam}/segment_*.ts`, sorts by embedded sequence number, keeps the newest `local_buffer_size + 60` (~30+ MB upper bound per camera), and removes the rest. Runs on `tokio::task::spawn_blocking` so large directories don't stall the poll loop. Unit tests in `hls_uploader.rs::tests` (`sweep_keeps_newest_segments_by_sequence`, `sweep_noop_when_below_keep_count`, `sweep_ignores_non_segment_files`, `sweep_handles_nonexistent_dir`) lock the behaviour in.

### Encoder coercion (`src/node/runner.rs:174-188`)

The Pi's `h264_v4l2m2m` hardware encoder writes a non-conforming SPS on every Pi hardware revision we've tested, so it's been retired across the codebase (see `HlsGenerator::detect_hw_encoder` for the full reasoning). Because the runner normally only re-detects the encoder when the DB value is empty, a Pi that completed setup on v0.1.12 would otherwise keep using `h264_v4l2m2m` forever.

The coercion works by walking `RETIRED_ENCODERS: &[&str] = &["h264_v4l2m2m"]` against the stored value; if any match, the DB value is cleared to force re-detection on the next start. New retirements only need a one-liner added to that slice.

### Video push path

```
Camera ─► FFmpeg muxer ─► data/hls/{cam}/segment_NNNNN.ts
                               │
                  hls_uploader │ detects new file
                               ▼
                      segment_uploader.push_segment()
                               │   bytes, filename
                               ▼
            POST /api/cameras/{cam}/push-segment?filename=…
            Header: X-Node-API-Key: …
            Body:   raw MPEG-TS bytes, Content-Type: video/mp2t
                               │
                               ▼
                     Command Center in-memory cache
```

On every playlist refresh (`stream.m3u8`), CloudNode also POSTs the file text to `POST /api/cameras/{id}/playlist`. The backend rewrites segment URLs to relative proxy paths and caches that rewritten version.

### Motion events

`motion_detector.rs` runs a second FFmpeg per camera with the `select='gt(scene,THRESHOLD)'` filter. Above-threshold frames emit a `MotionEvent { camera_id, score, timestamp, segment_seq }` onto an `mpsc` channel.

The WebSocket loop (`api/websocket.rs`) drains that channel inside its `tokio::select!`. When an event arrives it sends:

```json
{"type": "event", "command": "motion_detected", "payload": {...}}
```

If the WebSocket is disconnected, the event falls back to `POST /api/cameras/{id}/motion` via `ApiClient::report_motion()`. Cooldown is applied by `hls_uploader` before the event ever reaches the channel, so flapping is not a concern at the backend.

### Camera capture (platform-specific)

- **Linux:** `/dev/video*` devices via v4l2
- **Windows:** DirectShow via FFmpeg `-f dshow`
- **macOS:** AVFoundation via FFmpeg `-f avfoundation`

### HLS generation

FFmpeg subprocess transcoding camera → HLS segments:
- Output: `./data/hls/{camera_id}/stream.m3u8`
- Segment duration: `streaming.hls.segment_duration` (default 1s)
- Playlist window: `streaming.hls.playlist_size` (default 15)
- HLS directories wiped on startup so segment numbering resets cleanly
- Encoder detected once at startup and shared across all cameras

**Encoder-specific args** (`HlsGenerator::build_encoding_args`):

| Encoder | Accepts `-level auto`? | Preset | Notes |
|---------|------------------------|--------|-------|
| `h264_nvenc` (NVIDIA) | Yes | `p5` | CBR + zerolatency, `-level auto` lets NVENC pick |
| `h264_qsv` (Intel) | Yes | `veryfast` | CBR |
| `h264_amf` (AMD) | Yes | `speed` | CBR |
| `libx264` (CPU fallback) | **No** — level omitted | `ultrafast` | libx264 auto-computes level and embeds it in the SPS |

**HLS muxer flags** (`HLS_FLAGS_VALUE` in `hls_generator.rs`): passed to every FFmpeg invocation as `-hls_flags append_list`. `append_list` is required for the uploader's playlist-polling loop — without it FFmpeg truncates the playlist on every write and the uploader sees an empty file. We deliberately *omit* `delete_segments` (added in v0.1.16, removed in v0.1.17): on Windows its rotation-delete races AV scanners and NTFS's lazy-close and fires `failed to delete old segment ...` warnings on every rotation. Cleanup now lives in the `sweep_orphan_segments` path (see the FFmpeg supervisor section above) — by the time that 60s-cadence sweeper runs, transient handles have closed and `std::fs::remove_file` succeeds cleanly. The regression test `hls_flags_append_list_without_delete_segments` locks both decisions in (must contain `append_list`, must *not* contain `delete_segments`).

`-level auto` is a driver-specific string accepted only by the hardware encoders; passing it to libx264 errors with `Error parsing option 'level' with value 'auto'` and the encoder refuses to open. Omitting `-level` entirely lets libx264 compute the right level from resolution / framerate / bitrate and write it into the SPS — which is what hls.js / MSE needs to decode.

`-preset ultrafast` (not `veryfast`) is deliberate for the Pi 4 case: at 1080p30 ultrafast runs ~1.5 cores per stream, so two simultaneous cameras fit in the Pi 4's 4-core budget with headroom for the upload / dashboard / WebSocket tasks. `veryfast` is ~2-3 cores per stream and would starve the second camera on a Pi. The regression tests `libx264_args_omit_level_flag` and `libx264_args_use_ultrafast_preset` in `hls_generator.rs` lock both decisions in.

### Storage architecture

Three tiers, each with a distinct purpose and lifetime. The mental model:

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                                                                              │
│   TRANSIENT DISK                  CLOUD (primary)              LOCAL ARCHIVE │
│   data/hls/{cam}/*.ts             Command Center               data/node.db  │
│                                                                (SQLite, WAL) │
│   ────────────────                ──────────────               ───────────── │
│   1 s MPEG-TS segments            in-memory cache              snapshots     │
│   + stream.m3u8                   authoritative live           recording_segs│
│   newest ~30 kept                 backend rewrites URLs        config (AES)  │
│   swept every ~60 s               to proxy paths               logs (TUI)    │
│                                                                              │
│   ~12 MB/camera bounded           bounded by backend policy    bounded by    │
│                                                                storage       │
│                                                                .max_size_gb  │
│                                                                (default 64G) │
│                                                                              │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Every segment always flows to the cloud** — that is the live feed. The disk tier is a pure staging buffer. The SQLite tier is *additive*: populated only when the camera's recording policy says so (per-camera `continuous_24_7` or in-window `scheduled_recording`, reconciled from the heartbeat's `recording_state` map), layered on top of the existing cloud push.

#### Per-segment lifecycle

```
 ┌─ FFmpeg HLS muxer ──────────────────────────────────────────┐
 │   camera → H.264/AAC MPEG-TS → .ts (1 s) + stream.m3u8      │
 │   muxer flags: append_list (delete_segments deliberately    │
 │   omitted; see HLS generation section)                      │
 └─────────────────────────┬───────────────────────────────────┘
                           │ writes
                           ▼
             data/hls/{cam_id}/segment_NNNNN.ts
                           │
          hls_uploader.rs  │ polls playlist ~every 1 s
                           ▼
 ┌─ segment_uploader.push_segment() ───────────────────────────┐
 │   tokio::fs::read → bytes::Bytes → reqwest .body()          │
 │   POST {api_url}/api/cameras/{id}/push-segment?filename=…   │
 │   Header: X-Node-API-Key                                    │
 │   Content-Type: video/mp2t                                  │
 │   retry on 408/429/5xx, capped at 4 attempts (~4 s budget)  │
 └─────────────────────────┬───────────────────────────────────┘
                           │ Ok(true)
                           ▼
            ┌──────────────────────────────────┐
            │ Command Center in-memory cache   │  ◄── live feed
            │  (playlist pushed separately     │
            │   on every stream.m3u8 change)   │
            └──────────────────────────────────┘
                           │
                           │ back on the node, same task…
                           ▼
   ┌───────────────────────────────────────────────────────┐
   │  if recording_state[camera_id] == true:               │
   │      db.save_recording_segment(cam, seq, date, bytes) │
   │  always:                                              │
   │      tokio::fs::remove_file(segment_path)             │
   └───────────────────────────────────────────────────────┘

 ┌─ orphan sweeper (every ~60 s, spawn_blocking) ─────────────┐
 │   keep newest (local_buffer_size + 60) segments            │
 │   by sequence number; fs::remove_file the rest             │
 └────────────────────────────────────────────────────────────┘
```

#### SQLite schema (`data/node.db`)

Created in `src/storage/database.rs` with `PRAGMA journal_mode=WAL; synchronous=NORMAL;`:

| Table | Purpose | Populated by | Notes |
|-------|---------|--------------|-------|
| `snapshots` | JPEG BLOBs | `api/websocket.rs::cmd_take_snapshot` — on-demand WS `take_snapshot` command | FFmpeg extracts 1 frame from latest *complete* segment (via playlist, not FS scan — the current segment is still being written) |
| `recording_segments` | TS BLOBs | `hls_uploader.rs` inline, after successful cloud push, *only while* `recording_state[camera_id]` is set | Same bytes already in memory from the upload — no second disk read |
| `config` | KV store | Setup wizard + runtime updates | `api_key` stored via `set_config_encrypted` (AES-256-GCM); other keys plaintext |
| `logs` | Tracing events | `DashboardLayer` (logging.rs) | Survives restarts so the TUI shows prior history |

Indexes: `idx_snap_camera`, `idx_rec_camera_date`, `idx_logs_timestamp`.

**Why BLOBs, not loose files?** The original design kept snapshots and recordings in `data/snapshots/` and `data/recordings/` directories. Those were trivially copied off the box by anyone with filesystem access. Moving everything into SQLite BLOBs + encrypting the `api_key` column means lifting `data/node.db` off-node doesn't yield credentials. The key for AES-256-GCM is derived from the OS machine ID (`/etc/machine-id` on Linux, `MachineGuid` in HKLM on Windows, `IOPlatformUUID` on macOS), so the DB only decrypts on the machine that wrote it.

**BLOB encryption:** since v0.1.16, recording-segment and snapshot BLOBs are encrypted in addition to the API key. The pair is `encrypt_bytes` / `decrypt_bytes` in `storage/database.rs`. Wire format: `[5-byte magic "OSE\x02\x01"][12-byte nonce][ciphertext || 16-byte GCM tag]`. The magic prefix lets `decrypt_bytes` cleanly reject any blob that was never encrypted (legacy plaintext rows, accidental writes) instead of handing them to AES-GCM and surfacing a confusing tag-mismatch error. Decrypt failures are returned as a typed `DecryptError` enum (`BlobTooShort` / `NotEncrypted` / `WrongKeyOrCorrupted` / `KeyDerivation`) introduced in v0.1.17 so callers can log the root cause specifically — a stolen DB on a different machine produces `WrongKeyOrCorrupted`, while a legacy plaintext row produces `NotEncrypted`. See `docs/adr/0002-machine-id-encryption-key.md` for the full threat-model rationale and `docs/adr/0003-sqlite-recording-store.md` for why blobs live in SQLite at all.

#### Recording lifecycle

Recording is **opt-in and additive**. Live streaming is unconditional; recording layers BLOB archival on top while the flag is set.

State source-of-truth lives backend-side per-camera (`continuous_24_7`, `scheduled_recording`, `scheduled_start`, `scheduled_end` columns on `Camera`). Each heartbeat response carries an authoritative `recording_state: HashMap<camera_id, bool>` map computed by the backend from those columns + the org's wall-clock time. CloudNode reconciles its in-memory `recording_state` HashSet to exactly match the map every tick.

```
   HTTP heartbeat (node → backend, every 30s)              recording_state                SQLite
   ───────────────────────────────────────────             (in-memory HashSet)            ──────

   ┌──────────────────────────────────────┐
   │ POST /api/nodes/heartbeat            │
   │ { node_id, cameras, version, ... }   │ ──────▶ backend computes per-camera target
   └──────────────────────────────────────┘            from Camera.continuous_24_7 OR
                                                       (scheduled_recording AND in-window
                                                        per org timezone)
   ┌──────────────────────────────────────┐
   │ HeartbeatResponse                    │
   │ { recording_state: { cam_id: bool } }│ ──────▶ runner.rs reconciler:
   └──────────────────────────────────────┘             write lock on HashSet,
                                                       clear, insert all `true` cams
                                                            │
                                                            │ read per-segment in uploader
                                                            ▼
   ┌──────────────────────────┐                     ┌──────────────────┐
   │ normal uploader path     │ ──────────────────▶ │ save_recording_  │
   │ .ts pushed to cloud      │   if cam in set     │ segment(cam, seq,│
   │ bytes still in memory    │                     │ date, BLOB, size)│
   └──────────────────────────┘                     └──────────────────┘
```

**Self-healing across restarts**: a node that crashes loses its in-memory `recording_state` set, but the next heartbeat re-asserts the correct state from the backend's source of truth. No imperative WebSocket commands involved; the legacy `start_recording` / `stop_recording` WS arms were retired in v0.1.43.

The reconciler treats a missing `recording_state` field as "no info, leave the set alone" (older backend, transient hiccup) — that way a backend rollback or partial outage can't silently disable archive on every connected node.

#### Retention and cleanup

| Data | Owner | Trigger | Policy |
|------|-------|---------|--------|
| `data/hls/*.ts` files | `sweep_orphan_segments` (hls_uploader.rs) | Every ~60 s | Keep segments with the top-N highest sequence numbers (N = `local_buffer_size + 60`, default ≥ 30). `fs::remove_file` succeeds because transient handles have closed by sweep time. |
| DB size (all tables) | `enforce_retention` (database.rs) | On insert when over cap | Delete oldest `recording_segments` + `snapshots` until under `storage.max_size_gb` |
| `logs` table | `prune_logs` (database.rs) | Bounded row count | Keep newest K rows, delete rest |
| Credentials (`node_id`, `api_key`) | `prompt_for_reset` (setup/recovery.rs) | Interactive, after a failed registration | `DELETE FROM config WHERE key IN ('node_id','api_key')` via the live SQLite handle — not a file delete, so Windows' `FILE_SHARE_DELETE` race can't block it |

#### Recovering archived content

Recorded segments never leave the node through the local HTTP server — that server only serves the transient disk buffer (`data/hls/*`). Operators who want to pull archived clips go through the cloud:

- **Backend MCP tools** (`mcp__opensentry__get_incident_clip`, `mcp__opensentry__attach_clip`, etc.) fetch clips via the Command Center, which in turn queries the node through the WebSocket command channel.
- The node exposes `list_snapshots` and `list_recordings` commands in `api/websocket.rs::dispatch_command` — these return metadata rows from `snapshots` / `recording_segments`. Bulk retrieval of the BLOBs themselves currently runs through the cloud's own cache of pushed segments, not a per-BLOB fetch from the node; if a future incident-export feature needs the archived bytes directly, add a `get_recording` / `get_snapshot` handler next to those list commands.

### Local HTTP server (warp, port 8080)

Exposes the same `.ts` and `.m3u8` files the uploader pushes to the cloud, so you can stream locally without going through the backend (`VITE_LOCAL_HLS=true` on the frontend):

| Method | Path | Notes |
|--------|------|-------|
| GET | `/health` | Returns `OK` (also consumed by the Docker HEALTHCHECK) |
| GET | `/hls/{camera_id}/stream.m3u8` | Local HLS playlist |
| GET | `/hls/{camera_id}/segment_{n}.ts` | Local segment — filename must match `segment_<digits>.ts` exactly |

**Security:** the server has no authentication and binds to `127.0.0.1` by default. Only set `server.bind = "0.0.0.0"` if you explicitly want LAN-local HLS playback; changing it exposes live video to anyone on the network.

Recordings and snapshots used to be served here from the filesystem. They now live inside the encrypted SQLite DB and are fetched over the cloud API — the old `/recordings/*` and `/snapshots/*` routes were removed.

### Outbound API surface

All outbound calls use `ApiClient` in `src/api/client.rs`:

| Method | Path | Header | Body | When |
|--------|------|--------|------|------|
| POST | `/api/nodes/register` | `X-API-Key` | `RegisterRequest` JSON | Startup |
| POST | `/api/nodes/heartbeat` | `X-API-Key` | `HeartbeatRequest` JSON | Every `heartbeat_interval` s (fallback path; WS heartbeat is primary) |
| POST | `/api/cameras/{id}/codec` | `X-Node-API-Key` | `{video_codec, audio_codec}` JSON | After first segment or codec change |
| POST | `/api/cameras/{id}/push-segment?filename=…` | `X-Node-API-Key` | raw `.ts` bytes (`video/mp2t`) | Every segment |
| POST | `/api/cameras/{id}/playlist` | `X-Node-API-Key` | playlist text (`text/plain`) | Every playlist rewrite |
| POST | `/api/cameras/{id}/motion` | `X-Node-API-Key` | `{score, timestamp, segment_seq}` JSON | Motion event when WS is disconnected |
| WS | `/ws/node?api_key=…&node_id=…` | query params | JSON frames | Connected continuously; carries heartbeat, commands, motion events |

**WebSocket message types:**

- Node → Backend: `heartbeat`, `command_result`, `event` (with `command: "motion_detected"`)
- Backend → Node: `ack`, `command` (e.g. capture snapshot, start/stop recording), `error`

## Dashboard TUI (`src/dashboard/`)

Pre-split this was one 1,761-line file mixing data types, state mutations, ANSI rendering, slash-command dispatch, and the input event loop. Split into 6 focused files in commit `654e88e`:

| File | Holds |
|------|-------|
| `mod.rs` | Module routing + `pub use` re-exports — public API surface (`Dashboard`, `CameraState`, etc.) |
| `types.rs` | Pure data types — `LogLevel`, `LogEntry`, `CameraState`, `CameraStatus`, `View`, `SettingsInfo` |
| `state.rs` | `DashboardState` struct + state-mutation methods (`log`, `add_camera`, `record_upload`, …) + `CONFIRM_TIMEOUT` const |
| `handle.rs` | `pub struct Dashboard(pub Arc<Mutex<DashboardState>>)` + lifecycle/setup methods (`new`, `log_*`, `set_db`, `set_disabled_cameras`, `is_camera_suspended`, etc.) |
| `render.rs` | `Dashboard::render` + format helpers (panel rows, settings divider, plan badge, ANSI-aware truncation, box-drawing constants). Helpers are `pub(super)` so `commands.rs` can borrow `format_bytes` for the `/status` output. |
| `commands.rs` | `Dashboard::run_render_loop` (input event loop), `execute_command` (slash dispatcher), and the destructive-command confirm flow. The 7 `pending_confirm` unit tests live here next to `check_or_arm_confirm`. |

External callers see exactly the same API path: `crate::dashboard::Dashboard`, `crate::dashboard::CameraState`, etc. all still resolve through `mod.rs`'s re-exports, so `api/websocket.rs`, `logging.rs`, `node/runner.rs`, and `streaming/{hls_uploader,supervisor}.rs` were unaffected by the split.

Highlights:

- Full-screen live dashboard with camera status, upload stats, log viewer
- Slash command bar (`/help`, `/settings`, `/wipe`, `/export-logs`, `/reauth`, `/clear`, `/status`, `/quit`)
- Settings page with config display and action commands
- Raw mode input via crossterm events; `\x1B[nG` cursor positioning for right border alignment

### Destructive-command confirm-on-repeat

`/wipe` and `/reauth` don't execute the first time they're entered. They arm a `pending_confirm: Option<(command, Instant)>` on the dashboard; the **same command typed again within 30 seconds** (or the explicit `confirm` argument, e.g. `/wipe confirm`) actually runs it. Any unrelated command entered in between (including `/clear` or `/status`) drops the pending confirmation so an operator can't accidentally confirm a stale `/wipe` hours later.

The logic lives in `Dashboard::check_or_arm_confirm(cmd, explicit_arg, bare)` in `dashboard/commands.rs` and is exercised by the test block at the bottom of that file (look for `check_or_arm_confirm` assertions). Both commands also call `ApiClient::notify_wipe_started` / `notify_reauth` before the local action runs, so the backend logs the intent before the node potentially takes itself offline.

## Key Patterns

**Error Handling:** `thiserror` with custom `Error` enum in `src/error.rs`
```rust
pub type Result<T> = std::result::Result<T, Error>;
```

**Async Runtime:** `tokio` throughout
- All I/O is async
- FFmpeg managed via `tokio::process::Command`
- Channels (`tokio::sync::mpsc`) for motion events and command dispatch

**Platform Abstraction:** Conditional compilation
```rust
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
```

**FFmpeg binary:** `find_ffmpeg()` in `src/streaming/mod.rs` looks for `ffmpeg` on PATH only. The setup wizard offers to install via the OS package manager (`winget` / `brew` / `apt` / `dnf` / `pacman`) when missing — there is no bundled-FFmpeg path anymore (removed in v0.1.35).

**Retry policy:** `SegmentUploader` retries on 408/429/5xx and `reqwest` transport errors with exponential backoff (100ms, 200ms, 200ms).

## Development Workflow

1. **First Run:** `cargo run` → launches setup wizard
2. **Setup Wizard:** detects platform, cameras, verifies FFmpeg on PATH (offers `winget`/`brew`/`apt` install if missing), prompts for credentials, validates against `POST /api/nodes/validate`
3. **Config stored in DB:** saves to `data/node.db` (API key encrypted with AES-256-GCM)
4. **Subsequent Runs:** `cargo run` → loads config from DB, starts dashboard TUI

## Testing

**Unit tests:** `cargo test`
- 121+ unit tests across streaming / setup / node modules
- Integration tests in `tests/integration.rs`
- Uses `tokio-test` for async testing
- Key regression tests in `streaming/hls_generator.rs`:
  - `libx264_args_omit_level_flag` — guards against re-introducing `-level auto` on libx264
  - `libx264_args_use_ultrafast_preset` — locks the Pi 4 CPU budget
  - `hw_encoder_branches_still_use_level_auto` — makes sure the libx264 fix didn't break NVENC/QSV/AMF
  - `libx264_args_contain_required_pieces` — pix_fmt, codec, profile, audio
  - `hls_flags_include_delete_segments_and_append_list` — guards the v0.1.16 disk-fill fix (both flags must stay)
- Orphan-sweeper tests in `streaming/hls_uploader.rs`:
  - `sweep_keeps_newest_segments_by_sequence` — retention correctness
  - `sweep_noop_when_below_keep_count` — below-threshold no-op
  - `sweep_ignores_non_segment_files` — `stream.m3u8` / other files untouched
  - `sweep_handles_nonexistent_dir` — surfaces `io::Error` instead of panicking

**Manual probes (examples/):**

```bash
cargo run --example wsl_preflight_probe    # Print WSL + usbipd state without running setup
```

**Manual check:**
```bash
cargo run -- --once     # Run one detection cycle and exit (if supported by current main.rs)
```

## Docker

**Build:** `docker build -t sourcebox-sentry-cloudnode:latest .`

Published image: `ghcr.io/sourcebox-llc/opensentry-cloudnode`. Tags track the Cargo version (`:0.1.18`), plus floating `:latest` and `:0.1`. The image is built + pushed by `.github/workflows/release.yml` on tag push. Pi (ARM64) builds are source-only at the moment — no ARM image is published.

**Run:**
```bash
docker run -d \
  --device /dev/video0:/dev/video0 \
  -e SOURCEBOX_SENTRY_NODE_ID=xxx \
  -e SOURCEBOX_SENTRY_API_KEY=xxx \
  -e SOURCEBOX_SENTRY_API_URL=https://backend.example.com \
  -p 8080:8080 \
  -v ./data:/app/data \
  sourcebox-sentry-cloudnode:latest
```

**Docker Compose:** `docker-compose up -d`
- Requires `.env` with credentials
- Mounts `./data` for persistence

## Platform Notes

**Linux:** production-ready (v4l2)
- Add user to video group: `sudo usermod -a -G video $USER`
- Camera devices: `/dev/video0`, `/dev/video1`, etc.

**Raspberry Pi (Linux ARM64):** production-ready, build from source only
- Build: `cargo build --release` — no prebuilt binaries on the releases page for ARM
- Encoder: **libx264 CPU** only. `h264_v4l2m2m` is in `RETIRED_ENCODERS` because every Pi hardware revision we tested writes a non-conforming SPS that browsers reject.
- Preset: libx264 `ultrafast` keeps a 1080p30 stream under ~1.5 cores on a Pi 4, leaving room for a second camera. See "HLS generation" for the full rationale.
- USB: plug cameras into the Pi directly, not through an unpowered hub. Hub EMI faults show up as `usb-port: disabled by hub (EMI?)` + `xhci_hcd: Setup ERROR` in `dmesg` and wedge the whole USB controller until reboot.
- Under-voltage: `vcgencmd get_throttled` — anything non-zero means the PSU is sagging and FFmpeg restarts will follow.

**Windows:** production-ready (DirectShow)
- FFmpeg installed via `winget install Gyan.FFmpeg` (offered by the setup wizard when missing). No bundled copy.
- Camera names: `MEE USB Camera`, `Integrated Webcam`, etc.

**Windows + WSL2:** alternative deployment path
- Setup wizard detects the WSL2 option, runs `wsl_preflight.rs`, installs FFmpeg inside the chosen distro, and prints the `usbipd bind` / `usbipd attach --wsl` commands for each detected USB camera.
- Steps that need admin elevation (installing WSL itself, installing `usbipd-win` via `winget`, running `usbipd bind`) are printed for the operator to run in an elevated PowerShell — we don't execute them (Scope A). Scope B would handle elevation programmatically.
- `docker-desktop` distros are filtered out because they have no package manager and no v4l2 support.

**macOS:** experimental (AVFoundation)
- Requires FFmpeg: `brew install ffmpeg`
- May need camera permission in System Settings

## Key Dependencies

| Crate | Role |
|-------|------|
| `tokio` | Async runtime |
| `reqwest` | HTTP client (push-segment, playlist, motion, heartbeat) |
| `warp` | Local HTTP server |
| `tokio-tungstenite` + `futures-util` | WebSocket client |
| `serde` / `serde_json` | JSON serialization |
| `clap` | CLI parser |
| `tracing` / `tracing-subscriber` / `tracing-appender` | Logging |
| `crossterm` | Terminal raw mode + input events (dashboard TUI) |
| `inquire` / `indicatif` | Interactive prompts + progress bars (setup wizard) |
| `colored` | ANSI color formatting |
| `rusqlite` (bundled) | SQLite database |
| `aes-gcm` / `sha2` / `rand` | AES-256-GCM encryption for API key at rest (key derived from OS machine ID) |
| `bytes` | Zero-copy buffers for segment upload |
| `base64` | Snapshot image transfer over WebSocket |
| `percent-encoding` | URL-safe encoding for WebSocket query params with arbitrary key bytes |
| `once_cell` | Lazy one-shot static initialisation (logging registry, encoder cache) |
| `chrono` | Timestamps |
| `uuid` | Unique identifiers |
| `sysinfo` | System information (hostname, platform detection) |
| `anyhow` / `thiserror` | Error handling |
| `dotenvy` | Legacy `.env` loading |
| `zip` | Installer archive extraction (Windows FFmpeg download) |
| `libc` (Linux only) | Raw V4L2 `ioctl` for camera capability probing — see `src/camera/platform/linux.rs` |

## Code Conventions

- No `unwrap()` outside of tests — use `?` or an `Error` variant
- All errors use the custom `Error` enum (`src/error.rs`)
- Async functions return `Result<T>`
- Platform-specific code lives in `camera/platform/`
- Re-exports in `lib.rs` for convenience
- CLI subcommands handled in `main.rs`
