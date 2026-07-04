# Runbook: Live video isn't showing up in the dashboard

## When to use this runbook

A camera is registered (shows up in the dashboard grid) but the video tile is black, buffering forever, or showing "Stream not started yet". Use this runbook from the **Camera Node side** — once you've confirmed the node is the problem, not the Command Center.

If you haven't isolated the side yet, check the Command Center dashboard first: if the tile shows "No recent heartbeat" the backend isn't getting anything from the node at all — that's a connectivity / auth problem, not a streaming problem, and this runbook won't help.

## Prerequisites

- SSH or console access to the machine running the Camera Node.
- Permission to restart the node process.
- Network path from the Camera Node to the Command Center API URL.

## Step-by-step

### 1. Is the node process actually running?

```bash
# Linux / Pi / WSL2
systemctl status sourcebox-sentry-cameranode      # if installed as a service
ps aux | grep sourcebox-sentry-cameranode         # otherwise
```

If it isn't, start it and watch the first ~60s of logs:

```bash
RUST_LOG=info cargo run                    # dev
sourcebox-sentry-cameranode                       # release binary
```

A healthy startup ends with `[Node] Dashboard ready` and a row of `Streaming` lines — one per camera.

### 2. Is FFmpeg running for each camera?

On the dashboard TUI the camera rows should read `Streaming`. If they read `Restarting` or `Failed`, the FFmpeg supervisor has given up or is bouncing:

- `Restarting`: FFmpeg crashed but the supervisor is retrying with exponential backoff. Check the log panel for the exit status and stderr tail.
- `Failed`: FFmpeg exited more than 5 times in 60s and the supervisor marked the pipeline dead. The `effective_status` in the backend flips to offline. `/quit` the node, fix the root cause (see below), restart.

Common FFmpeg failures and what they mean:

| Exit status / log | Likely cause | Fix |
|-------------------|--------------|-----|
| `status: 234`, `[libx264] Error parsing option 'level'` | Camera Node < v0.1.15 on Pi / libx264 host | Update to v0.1.15+ (the fix that removed `-level auto` from libx264 args) |
| `Cannot open device /dev/video0` / errno -16 | Another process has the camera | Kill whatever's holding it (`fuser /dev/video0`), or unplug / replug |
| `Immediate exit after opening` | USB bandwidth starvation (multi-cam on a single bus, or a hub that can't hit USB 2.0 HS) | Plug cameras into separate root-hub ports; on Pi, connect directly to the board, not through a hub |
| `errno -28` / `(disk exhausted: N MiB free)` in the dashboard error tag | `data/hls/{cam}/` grew unbounded because the upload pipeline stalled and the orphan sweeper hasn't caught up | Update to v0.1.17+ — segment cleanup is now owned by the `sweep_orphan_segments` function in `hls_uploader.rs`, which runs every ~60 s and keeps the newest `local_buffer_size + 60` segments per camera (`-hls_flags delete_segments` was deliberately *removed* in v0.1.17 — FFmpeg's rotation-delete raced AV scanners on Windows and got `failed to delete old segment …` warnings on every rotation; our own sweeper handles cleanup more reliably). On a Pi, also `df -h ~/Sentinel-CameraNode/data` + `du -sh ~/Sentinel-CameraNode/data/hls/*` to confirm you're not still holding GBs of pre-v0.1.17 orphan `.ts` files — a `/wipe confirm` from the dashboard clears them. |

### 3. Are segments being pushed to the backend?

Filter the log panel for `push-segment`:

```
[SegmentUploader] push_segment(segment_00042.ts) → 200 OK in 47ms
```

If you see repeated non-200s:

- `401` / `403` — API key invalid or the node was deleted on the backend. Re-run `/reauth confirm` in the dashboard TUI.
- `404` — the camera was deleted on the backend but the node still has it in its config. Delete the camera row on the node (or `/wipe confirm` and redo setup).
- `413` — segment is >`SEGMENT_PUSH_MAX_BYTES` (backend default 2 MB). Drop `streaming.hls.bitrate` or `streaming.hls.segment_duration`.
- `429` — you're hitting the 1200/min rate limit per camera. Only happens if the encoder is producing sub-second segments; check `streaming.hls.segment_duration`.

No `push-segment` lines at all → FFmpeg isn't producing output. Re-check step 2.

### 4. Is the playlist being posted?

```
[HlsUploader] POST /api/cameras/<id>/playlist → 200 OK
```

If the playlist 200s but segments don't, the backend caches a playlist that points at segments it doesn't have — the browser fetches them, 404s, and the player stalls. This should self-heal within a few seconds once the segment uploader catches up; if it doesn't, the segment uploader has died — `/quit` and restart.

### 5. Does the backend cache look right?

From the Command Center side (admin-only `TestHlsPage` or direct curl):

```bash
curl -H "Authorization: Bearer $JWT" \
  https://sentinel-command.com/api/cameras/<id>/stream.m3u8
```

The response should contain a rolling list of `segment/segment_NNNNN.ts` entries (relative URLs, not presigned Tigris URLs). If it's empty or 404s, the backend never received a playlist push from the node — go back to step 4.

### 6. Can the browser actually decode the stream?

Open devtools → Network → filter on `.ts`. You should see 2xx responses every ~2 s. If the fetches succeed but the video is black:

- `#EXT-X-CODECS` in the playlist is missing or wrong. The codec is stamped by `POST /api/cameras/{id}/codec` after the first segment arrives — if the call hasn't landed yet the playlist omits the tag and some browsers (Safari especially) refuse to start playback.
- The SPS is non-conforming. Historically this was the Pi's retired `h264_v4l2m2m` encoder. Any node with that encoder in its config should auto-coerce to libx264 on startup (see `RETIRED_ENCODERS` in `src/node/runner.rs`). If it doesn't, manually clear the encoder value in `data/node.db` and restart.

## Rollback steps

If an update triggered the breakage, `cargo install --git https://github.com/SourceBox-LLC/Sentinel-CameraNode --tag v<previous>` to go back to the last known-good tag.

The config DB (`data/node.db`) is forward-compatible across the 0.1.x line; no migration rollback is needed.

## Escalation path

- If segments are pushing and the playlist is fresh but the browser still stalls, grab the playlist body, a segment, and a browser MSE console dump and open a bug at `SourceBox-LLC/Sentinel-CameraNode`.
- If multiple nodes in the same org are affected identically at the same time, it's almost certainly a backend regression — escalate to `SourceBox-LLC/Sentinel-Command` instead.

## See also

- Command Center README → "Troubleshooting → Live video never shows up in the dashboard" (the browser-side view of this same problem).
- `docs/adr/0001-pi-software-encoding.md` — why Pi is libx264-only and why `-level auto` is absent for that encoder.
- `AGENTS.md` → "FFmpeg supervisor" — the restart / stall-flag semantics.
