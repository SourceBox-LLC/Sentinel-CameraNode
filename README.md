<p align="center">
  <h1 align="center">SourceBox Sentry CloudNode</h1>
  <p align="center">
    Turn any USB webcam into a cloud-connected security camera.
    <br />
    <a href="#quick-start">Quick Start</a>
    &middot;
    <a href="#configuration">Configuration</a>
    &middot;
    <a href="#docker">Docker</a>
    &middot;
    <a href="#troubleshooting">Troubleshooting</a>
  </p>
</p>

<p align="center">
  <a href="https://www.gnu.org/licenses/gpl-3.0"><img src="https://img.shields.io/badge/License-GPLv3-blue.svg" alt="License: GPL v3"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Built_with-Rust-dea584.svg" alt="Built with Rust"></a>
</p>

---

CloudNode runs on your local network, detects USB cameras, and streams live video to the [SourceBox Sentry Command Center](https://opensentry-command.fly.dev) via HLS. All configuration is stored locally in an encrypted SQLite database — no cloud dependency for setup.

**What it does:**

- Detects USB cameras and transcodes each to HLS using FFmpeg (with hardware acceleration when available)
- Pushes 1-second `.ts` segments directly to Command Center's in-memory cache — no S3, no presigned URLs
- Detects motion from FFmpeg scene-change analysis and reports events over WebSocket (with an HTTP fallback for reliability)
- Stores recordings and snapshots locally in an encrypted SQLite database with automatic retention
- Runs a live terminal dashboard with slash commands and log viewer

**Supported platforms:**

| Platform | Status | Camera API |
|----------|--------|------------|
| Linux x86_64 / ARM64 | Production ready | Video4Linux2 |
| Windows 10 / 11 | Production ready | DirectShow |
| macOS | Experimental | AVFoundation |

---

## Quick Start

### Two install modes — pick one at setup time

CloudNode runs in one of two modes, chosen interactively by the setup wizard's first prompt:

- **Local-only** — free, runs on your home / office LAN, no account, no cloud.  Live camera viewing + snapshots + recording + recording playback through a browser dashboard at `http://<node-ip>:8080`.  Single LAN, single node.  Plex / Home Assistant / Synology model.
- **Connected** — pair with a [SourceBox Sentry Command Center](https://opensentry-command.fly.dev) account.  Adds multi-site dashboards, the Sentinel AI agent, mobile remote access, email alerts, MCP integrations, and team workflows.  Requires a free Command Center account and a node API key.

The same binary serves both modes; pick whichever fits.  Local installs can later be paired by re-running setup, but for now the choice is made once at first boot.

### Prerequisites

- A USB webcam
- **Connected mode only:** a [Command Center](https://opensentry-command.fly.dev) account with a Node ID and API Key (generated from the Settings page)
- **Docker** (recommended) or **Rust 1.70+** with **FFmpeg**

### Install

The fastest way to install CloudNode:

**Linux / macOS:**
```bash
curl -fsSL https://opensentry-command.fly.dev/install.sh | bash
```

**Windows:**

1. Download `sourcebox-sentry-cloudnode-windows-x86_64.msi` from the [latest release](https://github.com/SourceBox-LLC/opensentry-cloud-node/releases/latest).
2. Run the MSI (UAC prompt). SmartScreen will warn "Windows protected your PC" because the installer is unsigned — click **More info → Run anyway**. (Code signing is on the roadmap.)
3. From the Start menu, click **SourceBox Sentry CloudNode**. First launch runs the setup wizard interactively, then drops into the foreground TUI dashboard with cameras streaming. Every launch after just streams.

Config + recordings live under `C:\ProgramData\SourceBoxSentry\`. The setup wizard checks for FFmpeg and offers to install it via `winget install Gyan.FFmpeg` if it isn't already on PATH. For 24/7 unattended operation, the MSI also registers an optional Windows Service named `SourceBoxSentryCloudNode` (manual start by default) — see [Running as a Windows Service](#running-as-a-windows-service) below.

<details>
<summary><strong>Manual install (build from source)</strong></summary>

```bash
git clone https://github.com/SourceBox-LLC/opensentry-cloud-node.git
cd opensentry-cloud-node
cargo build --release

# Run the interactive setup wizard
./target/release/sourcebox-sentry-cloudnode setup
```
</details>

The setup wizard handles everything automatically:

1. Asks **"Connect this node to a Command Center?"** — `Yes` for the SaaS-paired mode, `No` for local-only.
2. Detects your platform and verifies connected cameras.
3. Verifies FFmpeg via your OS package manager — on Windows offers to run `winget install Gyan.FFmpeg`, on macOS `brew install ffmpeg`, on Linux prints the right `apt`/`dnf`/`pacman` command. CloudNode uses the system FFmpeg (no bundled copy).
4. **Connected mode only:** prompts for Node ID, API Key, and Command Center URL.
5. Detects the best available hardware encoder (NVENC, QSV, AMF).
6. Encrypts and stores credentials locally in the SQLite config DB (path varies by platform — see [Configuration](#configuration)).

After setup, start the node:

```bash
./target/release/sourcebox-sentry-cloudnode
```

The TUI status bar prints the local browser-dashboard URL (e.g.
`http://192.168.1.42:8080`).  Open that URL on any device on the same
LAN to see live camera feeds, take snapshots, toggle recording (Local
mode), and play back past recordings.

---

## Browser dashboard (Phase C+)

In addition to the in-terminal TUI, every CloudNode now serves a
React-based browser dashboard at `http://<node-ip>:8080/`.  In **Local
mode** it's the primary management surface; in **Connected mode** it
runs alongside the TUI as a viewer + diagnostic tool, with the
recording toggle disabled (Command Center is the source of truth).

What the browser dashboard does:

- **Cameras tab (`/`)** — live HLS grid, one tile per camera with
  snapshot + record-toggle buttons.  Refreshes every 5 s.
- **Recordings tab (`/recordings`)** — one cell per (camera, date).
  Click → modal player with HLS.js seeking through the encrypted
  SQLite blob store via `/api/recordings/{cam}/{date}/playlist.m3u8`.
- **Mode pill** in the header shows `Local` or `Connected` so the
  operator always knows which surface they're on.

Authentication: **none in v1.**  The server binds to `127.0.0.1` in
Connected mode (localhost-only) and to `0.0.0.0` in Local mode (any
device on the LAN).  See [docs/runbooks/local-mode-setup.md](docs/runbooks/local-mode-setup.md)
for the threat model and discovery options.

---

## Dashboard (TUI)

CloudNode also runs a full-screen terminal dashboard showing camera status, upload progress, and live logs.

Type `/` and press **Enter** to open the command menu.

**Main view:**

| Command | Description |
|---------|-------------|
| `/settings` | Open the settings page |
| `/status` | Show node status summary |
| `/clear` | Clear the log panel |
| `/quit` | Stop the node and exit |

**Settings page:**

| Command | Description |
|---------|-------------|
| `/export-logs` | Save logs to a timestamped file |
| `/wipe confirm` | Erase all stored data and reset |
| `/reauth confirm` | Clear credentials and re-run setup |
| `/back` | Return to the dashboard |

Press **Esc** to return from settings. Destructive commands (`/wipe`, `/reauth`) require confirmation: either press the command **twice within 30 seconds** — the first press arms the confirmation, the second executes — or pass the `confirm` argument explicitly (`/wipe confirm`). Any other command in between clears the armed state.

---

## Configuration

### How config is loaded

CloudNode resolves configuration in this order (highest priority last):

1. **SQLite database** — created by the setup wizard, primary source of truth. The database lives at:
   - **`$SOURCEBOX_SENTRY_DATA_DIR/node.db`** if the env var is set (Docker)
   - **`./data/node.db`** if it already exists (legacy / `cargo build` installs)
   - **`C:\ProgramData\SourceBoxSentry\node.db`** on Windows-MSI installs
   - **`./data/node.db`** otherwise (fresh manual install on Linux/macOS)
2. **YAML file** (`config.yaml`) — legacy fallback, auto-migrated to the DB on first load
3. **Environment variables** — override any stored values
4. **CLI flags** — highest priority

### Environment variables

Use environment variables to override database values without modifying the DB:

| Variable | Description |
|----------|-------------|
| `SOURCEBOX_SENTRY_NODE_ID` | Node ID |
| `SOURCEBOX_SENTRY_API_KEY` | API Key |
| `SOURCEBOX_SENTRY_API_URL` | Command Center URL |
| `SOURCEBOX_SENTRY_ENCODER` | Video encoder override (e.g. `h264_nvenc`, `libx264`) |
| `RUST_LOG` | Log level: `trace`, `debug`, `info`, `warn`, `error` |

### CLI flags

```bash
sourcebox-sentry-cloudnode --node-id <ID> --api-key <KEY> --api-url <URL>
```

### Motion detection

Motion detection is on by default. CloudNode pipes each camera through a second FFmpeg process with `select='gt(scene,THRESHOLD)'` to score how much the frame changed; scores above the threshold emit a `motion_detected` event (over WebSocket, or `POST /api/cameras/{id}/motion` if the socket is down). Per-camera cooldown prevents flapping.

Defaults (configurable via `config.yaml` `motion:` section):

| Setting | Default | Meaning |
|---------|---------|---------|
| `enabled` | `true` | Toggle motion detection |
| `threshold` | `0.02` | Scene-change score (0.0 = identical, 1.0 = totally different) |
| `cooldown_secs` | `30` | Minimum seconds between events per camera |

### Security

The API key is **encrypted at rest** using AES-256-GCM with a machine-derived key (SHA-256 of the OS machine identifier + application salt). CloudNode reads the identifier from `/etc/machine-id` on Linux, `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` on Windows, and `IOPlatformUUID` on macOS — values that are set once at OS install time, unique per host, and not user-modifiable. Moving `node.db` to a different host makes the stored key unreadable.

DBs written by older CloudNode versions that derived the key from the hostname are transparently re-encrypted with the new machine-ID-derived key on first load.

**Docker:** Alpine-based images don't ship with `/etc/machine-id`, so CloudNode generates a per-container ID on first run and stores it inside the mounted data volume (`$SOURCEBOX_SENTRY_DATA_DIR/.machine-id`). The ID persists across container rebuilds because it lives in the volume. For stronger encryption — a key tied to the host rather than the data volume — run the container with `-v /etc/machine-id:/etc/machine-id:ro`.

---

## Running on Windows

The Windows MSI installs CloudNode and creates a Start menu shortcut. The shortcut launches the binary as a **foreground TUI dashboard** — a console window opens with the live log, FFmpeg pushes segments, and the node stays online for as long as the window stays open. First launch detects no credentials and runs the setup wizard interactively before dropping into the dashboard; subsequent launches stream straight away.

This is the recommended path for everyday use: you can see what's happening, hit a slash command, and close it cleanly. For 24/7 unattended operation, see "Running as a Windows Service" below — the MSI registers an optional service for that case, but it's manual-start by default so you can confirm the foreground flow works first.

### Where things live

| Path | Purpose |
|------|---------|
| `C:\Program Files\SourceBox Sentry CloudNode\sourcebox-sentry-cloudnode.exe` | Binary (read-only after install) |
| `C:\ProgramData\SourceBoxSentry\node.db` | Encrypted SQLite — config + (optionally) recordings |
| `C:\ProgramData\SourceBoxSentry\logs\` | Daily-rotating service log files (text) — only written when running as a service |

### Reconfigure an enrolled node

Close the dashboard window, re-run setup, relaunch:

```powershell
sourcebox-sentry-cloudnode setup
# Then click the Start menu shortcut again, or run the binary with no args.
```

### Uninstalling

Use **Settings → Apps → SourceBox Sentry CloudNode → Uninstall**. The MSI uninstaller stops the service if it's running, removes the binary + service registration, and wipes `C:\ProgramData\SourceBoxSentry\` — including your encrypted config and recordings. FFmpeg installed via `winget` stays put because it's a separately-managed package.

> **Heads up:** the MSI is **unsigned** today. SmartScreen flags unsigned installers with "Windows protected your PC" — click **More info → Run anyway** to proceed. Code signing is deferred until we have a sustained release cadence worth the EV-cert fee; the binary itself is open source and reproducibly buildable from this repository if you want to verify.

## Running as a Windows Service

For 24/7 unattended operation (camera node in a closet, no one logged in), the MSI also registers an optional Windows Service named `SourceBoxSentryCloudNode`. The service runs as `LocalSystem` and is configured for **manual start** by default — you run setup first via the foreground path, confirm cameras stream, then flip the service to automatic.

### Enable auto-start on boot

```powershell
Start-Service SourceBoxSentryCloudNode
Set-Service -Name SourceBoxSentryCloudNode -StartupType Automatic
```

### Stop / restart / status

```powershell
Get-Service SourceBoxSentryCloudNode
Stop-Service SourceBoxSentryCloudNode
Restart-Service SourceBoxSentryCloudNode
```

### Tail the service log

```powershell
Get-Content -Wait "C:\ProgramData\SourceBoxSentry\logs\cloudnode-service.$(Get-Date -Format yyyy-MM-dd)"
```

If the service fails to start, the most common cause is that setup wasn't run or didn't complete. Check today's log file for a "CloudNode is not configured" error — that means `node.db` isn't where the service expects it.

> **Don't run the foreground TUI and the service at the same time** — only one process should hold the cameras. If you've enabled the service, close the dashboard window before letting the service start, or stop the service while you're running setup.

---

## Storage — where your video goes

Your live stream always goes to the cloud. A small rolling buffer stays on this machine while segments are being uploaded. Anything you explicitly **record** (and every snapshot you capture) is archived in an encrypted database on this machine — these are the only things with any real footprint.

```
             ┌────────── YOUR CAMERAS ──────────┐
             │                                  │
             ▼                                  ▼

  ┌─────────────────────────┐      ┌───────────────────────────┐
  │  data/hls/…/*.ts        │      │  Command Center (cloud)   │
  │                         │──┬──▶│                           │
  │  rolling 30-second      │  │   │  the live feed your       │
  │  buffer, ~12 MB per cam │  │   │  dashboard plays          │
  └─────────────────────────┘  │   └───────────────────────────┘
            │                  │
            │                  └── uploaded once per second, always on
            │
            │   if a camera is set to Record:
            │   save a copy into the archive below
            ▼

  ┌────────────────────────────────────────────────────┐
  │  data/node.db  — encrypted SQLite, one file        │
  │                                                    │
  │    • snapshots         (JPEG images)               │
  │    • recordings        (video chunks, opt-in)      │
  │    • config + API key  (AES-256-GCM, hardware-     │
  │                         bound so copying the       │
  │                         file off this machine      │
  │                         can't impersonate it)      │
  │    • logs              (recent dashboard history)  │
  │                                                    │
  │  Capped at storage.max_size_gb (default 64 GB).    │
  │  Oldest recordings and snapshots are deleted       │
  │  first when you hit the cap.                       │
  └────────────────────────────────────────────────────┘
```

### The three places your video can live

| Location | Holds | Retention | Size |
|----------|-------|-----------|------|
| **`data/hls/` on this machine** | The last ~30 seconds per camera, in 1-second video chunks | Rolling — swept every minute, keeps newest ~30 chunks | ~12 MB per camera at any moment |
| **Command Center (cloud)** | The live stream — what your dashboard actually plays | Per your Command Center plan | Handled by the backend |
| **`data/node.db` on this machine** | Snapshots, opt-in recordings, config, recent logs | Until it hits `storage.max_size_gb` (default 64 GB); oldest data purged first | Whatever you set |

### Recording is opt-in (per-camera)

Live streaming to the cloud is always on whenever the node is running — nothing to configure, nothing to toggle. **Recording** is a separate switch *per camera* that, when enabled, also saves each chunk into `data/node.db` so you have a local copy even if the cloud is unreachable later.

Three ways to enable it from Command Center:

- **Continuous 24/7** — toggle on per camera in Settings → Camera Nodes. Records all the time the node is online.
- **Scheduled** — toggle on per camera with a wall-clock window (e.g. 18:00–06:00). Times are interpreted in your org's timezone, set in Settings → Time Zone.
- **Manual** — the **Record** button on a camera tile in the dashboard. Same end state as Continuous 24/7 (it just flips the same flag).

The two policy modes are mutually exclusive — turning Continuous on auto-clears Scheduled in the same change, and vice versa.

**Self-healing across restarts** (v0.1.43+): the recording state lives in the backend's per-camera `continuous_24_7` / `scheduled_recording` columns. CloudNode reconciles its in-memory recording set from the heartbeat response every ~30s, so a node restart picks up the correct policy on its next heartbeat — operators don't have to re-enable anything after a power cycle.

### Snapshots

From the dashboard, **Take Snapshot** pulls one JPEG frame from that camera's most recent complete video chunk and saves it into `data/node.db`. You can list and view snapshots from the dashboard. They're also subject to the same retention cap.

### What's encrypted, and what isn't

**Encrypted at rest:** your API key, in the `config` table, using AES-256-GCM with a key derived from this machine's hardware ID (see the [Security](#security) section above for the full key-derivation story). Copying `data/node.db` to another machine makes the API key undecryptable there — an attacker can't impersonate your node just by lifting the file.

**Not encrypted at rest:** the video chunks and JPEG snapshots themselves. The assumption is that anyone with filesystem access to `data/node.db` can already watch the live stream from the same machine, so encrypting the BLOBs would add complexity without meaningfully raising the bar. If you need at-rest encryption for the video content too, put the `data/` directory on an encrypted volume — BitLocker on Windows, LUKS on Linux, FileVault on macOS.

### What happens when you're about to run out of disk

Two safety nets:

1. **Automatic retention.** Every 5 minutes the node checks total stored bytes against the operator-chosen `max_size_gb` cap. When the cap is exceeded, oldest recording segments are deleted first (FIFO) until usage is back under the cap. You won't get an error; the node just keeps running with fresh data. The Command Center dashboard surfaces a per-node usage bar so you can see it climbing toward the cap.

2. **Host-disk safety floor.** Cross-platform via `sysinfo`: when the underlying filesystem drops below 1 GiB free, the node *pauses durable recording writes* on the next heartbeat tick (regardless of how the cap was set). Live streaming continues unchanged; only the archive is paused. Recording resumes automatically once free disk recovers above the floor — typically because retention has freed up space, or the operator cleared other files.

### How to start fresh

Ordered from least to most destructive — pick the one that matches your goal:

- **Lower the cap.** Re-run the setup wizard and pick a smaller `max_size_gb`; retention will sweep oldest segments at the next 5-minute tick to fit the new cap.
- **Wipe recordings and snapshots but keep your credentials.** From the node's live dashboard (TUI), open the command bar and run `/wipe`. This clears the recording and snapshot tables in `data/node.db` and asks the backend to drop the node record; setup will re-pair on next launch if you want.
- **Reset credentials only.** If your node has the wrong ID or API key, the dashboard will surface a red "Registration Failed" screen offering to wipe credentials and re-launch the setup wizard. Accept it and you're back at step 1.
- **Full reinstall.** Stop the node and delete the `data/` directory. On next launch the setup wizard runs from scratch.

### Can I watch or download old recordings?

Yes — from the Command Center dashboard. Recorded video and snapshots show up in the camera's history view; the backend fetches them from this node's archive over the same channel it uses for everything else.

The node's own local HTTP server (`127.0.0.1:8080`) only serves the 30-second live buffer — it has no endpoint for reaching into the archive. That's deliberate: the archive is private to the node and reachable only through your authenticated Command Center session.

---

## Docker

Prebuilt multi-arch images (`linux/amd64`, `linux/arm64`) are published to GitHub Container Registry on every release tag.

### Quick run (prebuilt image — recommended)

```bash
docker pull ghcr.io/sourcebox-llc/opensentry-cloudnode:latest

docker run -d \
  --name sourcebox-sentry-cloudnode \
  --device /dev/video0:/dev/video0 \
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \
  -e SOURCEBOX_SENTRY_API_URL=https://your-backend.example.com \
  -p 8080:8080 \
  -v ./data:/app/data \
  ghcr.io/sourcebox-llc/opensentry-cloudnode:latest
```

Pin to a specific release instead of `:latest` when you want reproducible deploys — e.g. `ghcr.io/sourcebox-llc/opensentry-cloudnode:0.1.18`. Major.minor tags like `:0.1` are also published and float to the newest patch. See [releases](https://github.com/SourceBox-LLC/opensentry-cloud-node/releases) for the current version.

### Docker Compose

```bash
cp .env.example .env
# Edit .env with your credentials
docker-compose up -d
```

> The bundled `docker-compose.yml` currently builds from source (`build: .`). To use the prebuilt image instead, swap the `build:` line for `image: ghcr.io/sourcebox-llc/opensentry-cloudnode:latest` and run `docker compose pull && docker compose up -d`.

### Build from source (dev / airgapped)

```bash
docker build -t sourcebox-sentry-cloudnode .

docker run -d \
  --name sourcebox-sentry-cloudnode \
  --device /dev/video0:/dev/video0 \
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \
  -e SOURCEBOX_SENTRY_API_URL=https://your-backend.example.com \
  -p 8080:8080 \
  -v ./data:/app/data \
  sourcebox-sentry-cloudnode
```

### Multiple cameras

Pass each device to the container:

```bash
docker run -d \
  --device /dev/video0:/dev/video0 \
  --device /dev/video2:/dev/video2 \
  -e SOURCEBOX_SENTRY_NODE_ID=your_node_id \
  -e SOURCEBOX_SENTRY_API_KEY=your_api_key \
  -e SOURCEBOX_SENTRY_API_URL=https://your-backend.example.com \
  -p 8080:8080 \
  ghcr.io/sourcebox-llc/opensentry-cloudnode:latest
```

---

## Architecture

```
                  USB Cameras
                      │
              ┌───────┴─────────────────┐
              │       CloudNode         │
              │                         │
              │  Camera detection ──────┼──► FFmpeg (HLS transcoding)
              │                         │           │
              │  Motion detector ───────┼────┐      ▼
              │  (scene-change FFmpeg)  │    │   .ts + .m3u8
              │                         │    │      │
              │  Dashboard (TUI)        │    │      ▼
              │                         │    │   ┌─────────────────┐
              │  HTTP server :8080 ◄────┼────┼───│  SegmentUploader │──push─► Command Center
              │  (local HLS + recs)     │    │   └─────────────────┘   (POST /push-segment)
              │                         │    │
              │  WebSocket client ◄─────┼────┘   motion event
              │  (+ HTTP fallback)      │    ───────► POST /api/.../motion (if WS is down)
              └─────────────────────────┘
```

**Video pipeline:** Camera → FFmpeg subprocess → rolling HLS segments (`.ts`) → `SegmentUploader` pushes each segment to Command Center via `POST /api/cameras/{id}/push-segment` → backend caches in memory → browser fetches via same-origin proxy. **No S3, no presigned URLs.**

**Playlist:** Every time FFmpeg rewrites `stream.m3u8`, CloudNode also POSTs the playlist text to `POST /api/cameras/{id}/playlist` so the backend's rewritten (relative-URL) copy stays fresh.

**Motion:** A second FFmpeg probe per camera emits scene-change scores. Above-threshold frames raise a `MotionEvent`, which is sent over the WebSocket (`/ws/node`) as an `event { command: "motion_detected" }`. If the socket is disconnected, the uploader falls back to `POST /api/cameras/{id}/motion`.

**Local storage:** SQLite database (`data/node.db`) stores configuration, snapshots, and recordings as BLOBs (not exposed in open folders). Retention is enforced automatically — oldest data is deleted first when `max_size_gb` is exceeded. See the [Storage](#storage--where-your-video-goes) section for a full walkthrough of the three tiers (disk buffer, cloud, local archive) and how to manage them.

**Hardware encoding:** At startup, CloudNode probes for a hardware encoder (NVENC, QSV, AMF) and caches the result in the database. Falls back to `libx264` if none is found.

---

## API Endpoints

The node runs an HTTP server on port 8080 for local access:

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check (also used by the Docker `HEALTHCHECK`) |
| GET | `/hls/{camera_id}/stream.m3u8` | HLS playlist (local) |
| GET | `/hls/{camera_id}/segment_{n}.ts` | Video segment (local) — filename must be `segment_<digits>.ts` |

The server has **no authentication** and binds to `127.0.0.1` by default — only the local machine can reach it. If you need LAN-local HLS playback (e.g. `VITE_LOCAL_HLS=true` on the Command Center frontend from another device), set `server.bind = "0.0.0.0"` in your config — and understand you're exposing live video to everyone on that network. In normal cloud operation the browser fetches video through the backend proxy, not directly from the node.

Recordings and snapshots now live inside the encrypted SQLite database rather than on the filesystem; the old `/recordings/*` and `/snapshots/*` routes were removed.

**Outbound calls to Command Center** (via `reqwest`):

All authenticated outbound calls use the same header: **`X-Node-API-Key: <api_key>`**. The WebSocket is the only exception — it takes the key as a query parameter.

| Endpoint | Purpose |
|----------|---------|
| `POST /api/nodes/validate` | Validate a `node_id` + API key pair before saving config (setup wizard) |
| `POST /api/nodes/register` | Register node + cameras on startup |
| `POST /api/nodes/heartbeat` | Liveness (every 30s by default) |
| `POST /api/cameras/{id}/codec` | Report detected video/audio codec |
| `POST /api/cameras/{id}/push-segment?filename=…` | Push a `.ts` segment into the backend's in-memory cache |
| `POST /api/cameras/{id}/playlist` | Update the rewritten HLS playlist |
| `POST /api/cameras/{id}/motion` | Motion event fallback (used when WebSocket is down) |
| `WS /ws/node?api_key=…&node_id=…` | Bidirectional channel: heartbeat, commands, motion events (key passed as query param) |

---

## Development

### Build

```bash
cargo build              # Debug
cargo build --release    # Optimized
cargo test               # Run tests
cargo clippy             # Lint
cargo fmt -- --check     # Format check
```

### Project structure

```
src/
├── main.rs               # CLI entry point (clap)
├── lib.rs                # Library re-exports
├── dashboard.rs          # Live TUI dashboard + slash commands
├── error.rs              # Custom Error enum + Result type
├── logging.rs            # tracing subscriber setup
├── api/                  # Cloud API client + WebSocket
│   ├── client.rs         # ApiClient — register, heartbeat, codec, push-segment, playlist, motion
│   ├── websocket.rs      # WebSocket loop with auto-reconnect; sends motion events
│   ├── types.rs          # Request/response types
│   └── mod.rs
├── camera/               # Detection and capture (platform-specific)
│   ├── detector.rs       # Auto-detect USB cameras
│   ├── capture.rs        # Frame capture
│   ├── platform/         # Linux (v4l2) / Windows (DirectShow) / macOS (AVFoundation)
│   └── types.rs
├── config/               # Config loader (DB → YAML → env → CLI)
├── node/                 # Orchestration and lifecycle
│   └── runner.rs
├── server/               # Local HTTP server (warp) — health, HLS, recordings, snapshots
├── setup/                # Interactive TUI setup wizard (crossterm + inquire)
├── streaming/            # HLS pipeline
│   ├── hls_generator.rs   # FFmpeg subprocess per camera (HLS muxer)
│   ├── hls_uploader.rs    # Watches HLS dir, hands segments to SegmentUploader, updates playlist, drives motion events
│   ├── segment_uploader.rs# Posts each .ts to POST /push-segment with retry
│   ├── motion_detector.rs # Parallel FFmpeg scene-change scorer
│   └── codec_detector.rs  # FFprobe-based codec detection
└── storage/              # SQLite-backed local storage (BLOBs + config)
```

### Cross-compilation

```bash
# Raspberry Pi (ARM64)
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

---

## Platform Notes

<details>
<summary><strong>Linux</strong></summary>

Camera devices appear at `/dev/video*`. Add your user to the `video` group:

```bash
sudo usermod -a -G video $USER
# Log out and back in
```

Install FFmpeg:

```bash
sudo apt install ffmpeg        # Ubuntu / Debian
sudo dnf install ffmpeg        # Fedora
sudo pacman -S ffmpeg          # Arch
```

</details>

<details>
<summary><strong>Raspberry Pi (4 / 5, 64-bit)</strong></summary>

CloudNode runs on 64-bit Raspberry Pi OS. Build from source (the prebuilt ARM64 Docker image also works, but a native build skips the container USB-passthrough setup):

```bash
sudo apt install -y build-essential pkg-config libssl-dev ffmpeg
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
git clone https://github.com/SourceBox-LLC/opensentry-cloud-node.git
cd opensentry-cloud-node
cargo build --release
./target/release/sourcebox-sentry-cloudnode setup
```

The first `cargo build --release` on a Pi 4 takes 15–20 minutes. Subsequent incremental builds after `git pull` are 1–3 minutes.

**Software encoding only.** CloudNode deliberately does **not** use the Pi's `h264_v4l2m2m` hardware encoder — it produces a malformed SPS that the browser's Media Source Extensions layer rejects (video never appears, even though FFmpeg reports success). The node encodes with `libx264 -preset ultrafast` instead, which sustains 1080p30 at about 1.5 cores per camera on a Pi 4. A Pi 4 comfortably runs 2 cameras at 1080p30; a Pi 5 runs 3–4.

**USB cameras.** Plug webcams **directly into the Pi's USB ports** rather than through a hub when possible — unpowered hubs frequently brown out under the combined draw of two UVC cameras streaming at 1080p, and a hub fault can wedge the entire xhci controller until reboot. Use the blue USB 3.0 ports (top pair) for more power budget even if the camera only needs USB 2.0 bandwidth.

**Thermal.** Two simultaneous `libx264` streams will push a bare Pi 4 past 80 °C and trip the kernel's thermal throttle, dropping frame rate. A heatsink and fan are a small upgrade that make this non-issue. Check with `vcgencmd measure_temp` and `vcgencmd get_throttled` (anything other than `throttled=0x0` indicates a power or thermal event).

</details>

<details>
<summary><strong>Windows</strong></summary>

CloudNode runs natively on Windows using DirectShow. FFmpeg is **not** bundled — the setup wizard checks PATH and offers to run `winget install Gyan.FFmpeg` if it isn't already installed.

Camera names (e.g. `MEE USB Camera`, `Integrated Webcam`) are detected via DirectShow enumeration.

**WSL2 deployment (optional):** The setup wizard on Windows can also deploy inside WSL2 (useful for Linux-native builds and tighter V4L2 integration). The wizard's WSL preflight detects whether WSL is installed, finds a usable distro, installs FFmpeg inside it, and prints the `usbipd bind` / `usbipd attach --wsl` commands you need to run in an admin PowerShell to forward USB cameras from the host into the distro. Elevation-required steps (installing WSL, installing `usbipd-win`, `usbipd bind`) are printed for the operator to run rather than executed on their behalf.

</details>

<details>
<summary><strong>macOS</strong></summary>

Install FFmpeg via Homebrew:

```bash
brew install ffmpeg
```

You may need to grant camera access in **System Settings > Privacy & Security > Camera**.

</details>

---

## Troubleshooting

For the full end-to-end "live video isn't showing up in the dashboard" workflow, see [`docs/runbooks/video-not-showing.md`](docs/runbooks/video-not-showing.md). The most common causes are also captured below.

<details>
<summary><strong>No cameras detected</strong></summary>

**Linux:** Verify device exists and permissions are correct:

```bash
ls -l /dev/video*
# Should show crw-rw---- with group 'video'
```

Add your user to the video group if needed:

```bash
sudo usermod -a -G video $USER
```

**Windows:** Ensure the camera is not in use by another application (Zoom, Teams, etc.).

</details>

<details>
<summary><strong>FFmpeg not found</strong></summary>

**Windows:** Install FFmpeg via `winget install Gyan.FFmpeg` (or download from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) and add to PATH), then re-run `sourcebox-sentry-cloudnode setup`. CloudNode looks for FFmpeg on PATH only.

**Linux / macOS:** Install FFmpeg using your package manager (see [Platform Notes](#platform-notes)).

</details>

<details>
<summary><strong>HLS stream not playing</strong></summary>

1. Verify the node is running: `curl http://localhost:8080/health`
2. Check the dashboard for FFmpeg errors
3. Confirm HLS files are being created in `data/hls/`
4. Watch the dashboard log for `Pushed segment …` lines — those mean segments are reaching the backend
5. Try `/export-logs` from the settings page for detailed diagnostics

</details>

<details>
<summary><strong>Cannot connect to Command Center</strong></summary>

1. Verify your API URL: `curl https://your-backend.example.com/api/health`
2. Open `/settings` in the dashboard to confirm Node ID and API URL
3. Use `/reauth confirm` from settings to re-enter credentials

</details>

<details>
<summary><strong>Motion events not firing</strong></summary>

1. Check that `motion.enabled` is `true` (default)
2. Lower `motion.threshold` (default `0.02`) if the scene is dim / low-contrast
3. The dashboard logs `Motion detected on <camera> (score N%)` when an event fires

</details>

<details>
<summary><strong>Docker container can't access camera</strong></summary>

Pass each camera device explicitly:

```bash
docker run --device /dev/video0:/dev/video0 ...
```

</details>

<details>
<summary><strong>FFmpeg exits with status 234 in a restart loop (encoder open failure)</strong></summary>

The dashboard log shows repeated `FFmpeg exited with exit status: 234` and messages like `Could not open encoder before EOF` or `Error parsing option '...' with value '...'`. This means the selected encoder can't be initialized with the current argument set.

1. Look for the line `Selected encoder: <name>` or `Using software encoder (configured)` in the startup log to see which encoder was picked.
2. If the encoder is a hardware codec (`h264_nvenc`, `h264_qsv`, `h264_amf`) and the driver on this machine is broken, override with software: set `SOURCEBOX_SENTRY_ENCODER=libx264` or change it via the setup wizard.
3. On CloudNode ≥ v0.1.14 the `h264_v4l2m2m` Pi codec is automatically retired — a stale DB entry naming it is cleared on next launch (look for `Retired encoder '…' in config — clearing for re-detection`). If you're on an older node, run `/wipe` from the dashboard's settings page to clear the cached encoder and let auto-detect re-run.
4. If libx264 itself crashes with `Error parsing option 'level' with value 'auto'`, upgrade — that was a bug in the libx264 branch of `build_encoding_args` fixed in v0.1.15.

</details>

<details>
<summary><strong>Raspberry Pi: cameras drop off the USB bus under load</strong></summary>

Symptoms: cameras appear at startup then disappear after minutes/hours; `lsusb` shows only root hubs; `dmesg` shows `usb usb1-port1: disabled by hub (EMI?)` or `device descriptor read/64, error -110` or `xhci_hcd ... Setup ERROR`.

This is almost always a USB hub fault or power issue, not a software problem.

1. **Reboot** (`sudo reboot`). The xhci controller can wedge in a state hot-replugging doesn't recover; only a kernel restart clears it.
2. **Plug cameras directly into the Pi's ports** — remove any external hub, splitter, or extension cable from the path. Unpowered hubs routinely brown out under two 1080p UVC cameras.
3. **Check power** — `vcgencmd get_throttled` must return `0x0`. Any non-zero value means under-voltage or over-current events have happened; use the official 5V/3A USB-C supply.
4. **Try USB 3.0 (blue) ports** — more power budget than USB 2.0 (black) even for USB 2.0 devices.
5. **Verify post-reboot** — `lsusb` should show your camera's VID:PID (e.g. `1bcf:2283 Sunplus ... MEE USB Camera`), and `/dev/video0` / `/dev/video2` should exist.

If a full power cycle, direct-to-Pi connection, and official PSU all fail, the camera itself is the most likely cause — test each camera alone on the Pi before replacing hubs.

</details>

<details>
<summary><strong>Video plays in the dashboard but browser shows a black frame</strong></summary>

The CloudNode dashboard's `STREAMING` status and the ↑ segs counter only prove that segments are being produced and pushed to the backend — not that they're decodable by the browser's MSE (Media Source Extensions) layer.

1. In the Command Center dashboard, check the camera card's codec badge. A valid line like `avc1.42e01f` (Baseline, Level 3.1) or `avc1.4d401e` (Main, Level 3.0) means the SPS is parseable. A missing codec badge or `avc1.000000` means the encoder produced a malformed bitstream.
2. On the node, run `ffprobe` against a recent segment:
   ```bash
   ls -t data/hls/*/segment_*.ts | head -1 | xargs ffprobe 2>&1 | head -20
   ```
   Valid output shows a recognizable `Profile` (Baseline / Main / High) and a positive `Level` (e.g. `Level: 31`). If you see `Profile: unknown` or `Level: -99`, the encoder is producing garbage — force `libx264` as described in the encoder-crash entry above.

</details>

---

## License

Licensed under the [GNU General Public License v3.0](LICENSE).

CloudNode uses GPL-3.0 to ensure users can always inspect, modify, and verify what runs on their cameras. For commercial licensing, contact [SourceBox LLC](https://github.com/SourceBox-LLC).

---

<p align="center">
  <a href="https://opensentry-command.fly.dev">SourceBox Sentry Command Center</a>
  &middot;
  Made by the SourceBox Sentry Team
</p>
