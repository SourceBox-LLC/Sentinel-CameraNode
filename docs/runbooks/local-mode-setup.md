# Local-mode setup runbook

CloudNode can run **standalone on your LAN** with no Command Center
pairing.  This runbook covers what Local mode does, how to find the
node URL, the threat model, and how to upgrade later.

## What Local mode does

| Feature | Local mode | Connected mode |
|---|:---:|:---:|
| Live HLS playback (browser tile grid) | ✅ | ✅ |
| Snapshot capture | ✅ | ✅ |
| Continuous recording | ✅ (toggle in browser) | ✅ (toggle in Command Center) |
| Recording playback | ✅ | ✅ |
| Motion detection (FFmpeg scene scoring) | ✅ | ✅ |
| Encrypted SQLite recording storage | ✅ | ✅ |
| Multi-node aggregation | ❌ | ✅ |
| AI agent / Sentinel | ❌ | ✅ |
| MCP integrations (Claude, Cursor, custom agents) | ❌ | ✅ |
| Email alerts | ❌ | ✅ |
| Multi-user / org / role management | ❌ | ✅ |
| Mobile remote access (anywhere) | ❌ (LAN only) | ✅ |

**Local mode is a real product on its own.** The standalone feature
set is comparable to Plex's local-only mode or a basic Synology NAS.
You can defer the Command Center decision indefinitely; pairing later
just requires re-running setup.

## Browser dashboard tabs

The SPA at `http://<node-ip>:8080/` has three tabs in both modes:

- **Cameras (`/`)** — live HLS grid, one tile per camera with
  Snapshot + Record buttons.  Refreshes every 5 s.  Local mode shows
  a centered tile when only one camera is registered; the Record
  toggle is interactive in Local mode and disabled with a "Managed
  by Command Center" tooltip in Connected mode.
- **Snapshots (`/snapshots`)** — gallery of every JPEG you've
  captured, click-to-zoom modal, per-tile Delete button.  The image
  bytes come from the encrypted SQLite blob store — same archive the
  Command Center sees in Connected mode.
- **Recordings (`/recordings`)** — one cell per `(camera, date)`
  bucket.  Click → modal HLS.js player seeking through the dynamic
  VOD playlist for that day.

Local-mode installs additionally see a "Get more out of your cameras"
footer at the bottom of every page linking to Command Center.
Connected installs don't see it.

## Picking Local at install time

Run setup interactively:

```bash
sourcebox-sentry-cloudnode setup
```

The first wizard prompt asks:

> **Connect this node to a Command Center?**
>
> Yes — Connected mode (full SaaS).  No — Local-only mode (LAN-only, no account).

Pick `No`.  The wizard skips the API URL / Node ID / API Key prompts
and goes straight to deployment method + storage cap.  After setup
finishes, the node boots and the TUI status bar prints the LAN URL,
e.g.:

```
Node: a3f81c9d  [LOCAL]  │  http://192.168.1.42:8080  │  ↑ 0 segs  ⏱ 0:00:01
```

## Finding the node URL

The TUI status bar always shows it.  If you're SSH-ing into the box
or running headless, three options:

1. **TUI status bar.**  Connect a monitor (or SSH and run the binary
   in the foreground) — the URL is the second column of the status
   bar.
2. **Router admin page.**  Most home routers have a connected-devices
   list; the node shows up as the host's hostname.
3. **`arp -a` from any device on the LAN.**  Look for the host's MAC
   address and pair it with the IP you find.

mDNS auto-discovery (`opensentry-node.local`) is **not** in v1 —
that's planned for a future release.

## Threat model — no auth in v1

CloudNode does not require a username/password to access the browser
dashboard or the `/api/*` endpoints.

- **Connected mode default (`bind = 127.0.0.1`):** only same-host
  processes can hit the local server.  Anyone with shell access on
  the box could already wipe `data/node.db` directly, so the
  additional surface is zero.
- **Local mode default (`bind = 0.0.0.0`):** anyone on the LAN can
  read live HLS, snapshots, recordings, and toggle the local
  recording flag.  Acceptable for v1's home / small-business LAN
  target.  **Do not expose this server to the public internet.**

If you need LAN auth in v1, the workaround is to bind to a non-default
port and put a basic-auth reverse proxy in front (Caddy, nginx,
Tailscale-funnel-with-auth).  Built-in auth is on the post-v1 roadmap.

## Recording-toggle behavior in each mode

| Mode | Source of truth | Browser toggle behavior |
|---|---|---|
| Local | The `local_recording_state` SQLite table on this node.  Persists across restarts. | Click works; the toggle flips immediately and persists. |
| Connected | The Command Center's heartbeat reconciler.  CC's database is canonical. | Toggle is disabled with a `Managed by Command Center` tooltip.  The endpoint also returns `409 Conflict` defense-in-depth — flip the camera's recording policy in the CC UI instead. |

## Upgrading from Local → Connected later

Phase A's mode flag is set once at install time.  To switch a Local
install to Connected:

```bash
sourcebox-sentry-cloudnode setup
```

This re-runs the wizard.  Pick `Yes` at the mode prompt and supply
the Command Center credentials.  Existing recordings stay in the
encrypted SQLite store; the CC layers new features on top via the
heartbeat without touching local data.  Pairing is reversible —
`setup` can flip the node back to Local mode at any time.

## Uninstalling in Local mode

Same as Connected:

```bash
sourcebox-sentry-cloudnode uninstall
```

Local mode has no Command Center registration to clean up, so the
uninstaller skips the decommission step automatically.

## Troubleshooting

### The browser dashboard doesn't load
- Confirm the TUI shows `MODE: LOCAL` and a non-empty URL.
- Confirm your device is on the same LAN as the node (same Wi-Fi
  / same VLAN).
- Try `curl http://<node-ip>:8080/health` — should return `OK`.
- Check the node's host firewall: port 8080 must be open inbound on
  `0.0.0.0` for LAN access.  On Linux: `sudo ufw allow 8080`.  On
  Windows: the WiX MSI registers a firewall rule automatically; for
  manual installs add one with `netsh advfirewall firewall add rule`.

### "Recording managed by Command Center" tooltip
That's expected in Connected mode.  Flip the camera's recording
policy in the Command Center UI (Settings → Cameras) and the node's
heartbeat reconciler picks it up within ~30 seconds.

### Recording playback shows "no segments for camera+date"
Recording is opt-in and per-camera.  In Local mode, click the
**Record** button on the camera tile and wait at least one segment
(~1 s by default) to see something in the Recordings tab.  In
Connected mode, set the camera's recording policy in CC.

### "Snapshot failed: No segments found for camera"
Only seen on the very first ~1-2 s after a camera starts (FFmpeg
hasn't written segment 00000 yet).  Wait for the camera tile to show
the live feed, then click Snapshot again.  Steady-state captures are
reliable from v0.1.50 onward in Local mode and from v0.1.56 onward in
Connected mode — the per-task segment cleanup that used to race the
snapshot grab is now skipped in both modes (the orphan sweeper handles
disk pressure on a 60 s cadence with ≥30 segments retained,
~12-20 MB/camera).

### Connected mode: clicked Record in CC but local Recordings tab stays empty

Use the TUI's per-heartbeat diagnostic line (v0.1.60+) to figure out
which surface has the bug.  Every heartbeat (~30 s) logs one of:

| Log line | Meaning |
|---|---|
| `Heartbeat: 1 cam in policy (1 on)` | CC says: record this camera.  If no archive happens, check for `Recording archive: …` warn lines next. |
| `Heartbeat: 1 cam in policy (0 on)` | CC isn't flipping `continuous_24_7` on the row this heartbeat reads.  CC-side bug — re-click Record, verify the toast says "Recording started", check your CC user has admin role on the org. |
| `Heartbeat: no cameras in policy` | This camera isn't attached to the heartbeating node in CC's DB.  Wipe + re-pair the node, or delete the orphan camera from CC's Settings. |
| `Heartbeat: recording_state absent` | Older CC backend that doesn't send the field.  Upgrade Command Center. |

When the state transitions, the reconciler also logs
`Recording started — <camera_id> (per Command Center)` or
`Recording stopped — <camera_id>` exactly once per transition
(steady-state heartbeats produce empty diffs and no transition line).
If you see `(1 on)` plus the transition log but no segments appear in
the local Recordings tab within ~30 s, look for `Recording archive: …`
warn lines — they surface FS / DB write failures that pre-v0.1.59
were silent.

### "Snapshot captured but archive skipped — host disk is critically low"
The host filesystem dropped below the 1 GiB safety floor.  The
capture itself succeeded but the durable DB write was skipped to
avoid filling the disk further.  Free space in the data directory
(see Storage section in the README) and try again — recording resumes
automatically once free disk recovers above the floor.

### Node won't start in Local mode after setup
Check `data/node.db` has a `mode='local'` row:

```bash
sqlite3 data/node.db "SELECT key, value FROM config WHERE key = 'mode'"
```

If absent, the wizard didn't finish writing — re-run `setup`.
