# ADR 0001: Raspberry Pi uses libx264 software encoding, not h264_v4l2m2m

- **Status:** Accepted (v0.1.15)
- **Date:** 2026-04
- **Deciders:** Camera Node maintainers

## Context

Raspberry Pi's official hardware H.264 encoder is `h264_v4l2m2m`, which wraps the VideoCore IV / VI block on Pi 3 / Pi 4 / CM4. It exists in FFmpeg mainline and Ubuntu's FFmpeg packages, and on paper it should be the right choice for a 4-core ARM box that also needs to run a USB pipeline, a motion detector, an HTTP server, and a WebSocket client concurrently.

In practice, across every Pi hardware revision we tested, `h264_v4l2m2m` writes a **non-conforming SPS** (the H.264 Sequence Parameter Set that tells a decoder the resolution, profile, and level of the stream). Browsers relying on the Media Source Extensions (MSE) path refuse to initialise playback. The stream decodes fine in ffmpeg / mpv / VLC, so it looks healthy from a logs standpoint — the failure mode is silent from the node's perspective.

Symptoms:

- Browser: `HlsPlayer` enters a buffering loop; console shows `NOT_SUPPORTED_ERR` or `MEDIA_ERR_DECODE`.
- Backend: segments cache normally; `GET /stream.m3u8` returns a valid playlist; `GET /segment/*.ts` returns bytes.
- Ffmpeg / VLC: plays fine, so nothing in the capture chain looks wrong.
- Older versions of hls.js tolerated the broken SPS; newer versions (and all native Safari + Chrome MSE paths) reject it.

We considered patching the SPS in-stream (strip-and-rewrite the NAL unit before handing it to the HLS muxer) but:

- It requires parsing the bitstream in Rust, which adds a meaningful codepath to maintain.
- The fix is Pi-specific and drifts with every new FFmpeg release; patches that work on FFmpeg 4.4 don't necessarily work on 6.0.
- We'd still be stuck with the latency characteristics of `h264_v4l2m2m`, which varies wildly by firmware revision.

## Decision

**Raspberry Pi always uses libx264.** Encoder auto-detection (`HlsGenerator::detect_hw_encoder`) does not consider `h264_v4l2m2m` as a candidate. `h264_v4l2m2m` is explicitly listed in the `RETIRED_ENCODERS` slice in `src/node/runner.rs` so that config DBs written by older Camera Node versions (≤ v0.1.12) which may have stored `h264_v4l2m2m` as the picked encoder will clear that value on startup and force re-detection.

The libx264 FFmpeg args used on Pi are:

- `-preset ultrafast` — not `veryfast`. At 1080p30 ultrafast runs ~1.5 cores per stream; veryfast is ~2-3 cores. With two simultaneous cameras, veryfast starves one of them on a Pi 4. Validated by the `libx264_args_use_ultrafast_preset` regression test.
- **No `-level` flag.** libx264 doesn't accept `-level auto` (that's a driver-specific string only NVENC / QSV / AMF accept). Omitting it entirely lets libx264 compute the right level from resolution, framerate, and bitrate and embed it in the SPS — which is what hls.js / MSE needs to decode. Validated by the `libx264_args_omit_level_flag` regression test.
- `-profile:v main` — baseline is not guaranteed to ship audio tags in the way Safari's MSE expects, and high is overkill for a 2 Mbps stream.
- `-tune zerolatency` + `-sc_threshold 0` + `-g <fps>` — locks the GOP at exactly one second so HLS segment duration has something to round to.

## Consequences

**Positive:**

- Every Pi revision produces browser-compatible streams out of the box. No per-hardware special cases.
- Regression tests lock in both the preset and the absent `-level` flag, so a future "let's clean this up" refactor can't silently re-introduce the bug.
- `RETIRED_ENCODERS` gives us a one-liner to retire any future encoder that turns out to emit broken bitstreams.

**Negative:**

- CPU cost. A Pi 4 can run two simultaneous 1080p30 streams on libx264 ultrafast with ~1 core of headroom. A Pi 3 can handle one 720p30 stream comfortably; two is marginal. Pi Zero 2W is single-camera only.
- Power draw. Software encoding burns more watts than the VideoCore block would. On a battery-backed deployment this matters.
- Thermal throttling under sustained load. `ultrafast` is specifically chosen to stay below the Pi 4's sustained thermal ceiling at 1080p30, but a Pi in a closed case without a heatsink can still throttle. The supervisor's stall-flag watchdog (see `AGENTS.md → FFmpeg supervisor`) will catch a throttled FFmpeg that stops producing segments and route it through the normal restart path.

**Neutral:**

- x86 hosts are unaffected — NVENC / QSV / AMF paths are the primary encoders there and libx264 is only a fallback. The fix-for-Pi also quietly fixed the libx264 fallback on x86 (where `-level auto` was the same bug, just rare because NVENC almost always wins).

## Revisiting this decision

Re-evaluate if any of the following become true:

- FFmpeg upstream fixes the `h264_v4l2m2m` SPS bug. (We should pin-test against every Pi board we still support; don't just trust the release notes.)
- A new hardware encoder ships on a newer Pi revision (CM5, Pi 5, etc.) that we haven't tested yet.
- We introduce a feature that requires >2 simultaneous streams per Pi, making the CPU budget untenable.

In any of those cases, the decision can be flipped back by:

1. Adding the candidate encoder to `detect_hw_encoder`'s probe list.
2. Removing it from `RETIRED_ENCODERS`.
3. Adding regression tests that verify the SPS it emits parses cleanly through an MSE-style decoder (not just through FFmpeg).

## References

- `src/streaming/hls_generator.rs` → `build_encoding_args`, `detect_hw_encoder` — the encoder selection + args.
- `src/node/runner.rs` → `RETIRED_ENCODERS` coercion (in `run_internal`).
- `docs/runbooks/video-not-showing.md` → the operator's-view of this same bug class.
- v0.1.15 release notes — shipped this decision after the Pi regression.
