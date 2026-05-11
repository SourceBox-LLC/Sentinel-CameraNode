// Camera grid — live HLS playback per tile + snapshot button +
// record toggle.  Polls /api/cameras every 5 s so status pills
// reflect what the dashboard would show.

import { useCallback, useEffect, useState } from "react"
import { useOutletContext } from "react-router-dom"

import HlsPlayer from "../components/HlsPlayer"
import {
  Camera,
  COMMAND_CENTER_URL,
  listCameras,
  NodeStatus,
  setRecording,
  takeSnapshot,
} from "../lib/api"
import { useToasts } from "../lib/toasts"

const CAMERA_POLL_MS = 5_000

export default function CamerasPage() {
  const status = useOutletContext<NodeStatus | null>()
  const { showToast } = useToasts()
  const [cameras, setCameras] = useState<Camera[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  // Optimistic local state for the record toggle.  Server-side this
  // returns 409 in Connected mode — the UI greys the toggle out so
  // the user doesn't try, but a defensive catch handles 409s anyway.
  const [pendingRecord, setPendingRecord] = useState<Set<string>>(new Set())

  const refresh = useCallback(async () => {
    try {
      const list = await listCameras()
      setCameras(list)
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void refresh()
    const id = setInterval(refresh, CAMERA_POLL_MS)
    return () => clearInterval(id)
  }, [refresh])

  const onSnapshot = async (cam: Camera) => {
    try {
      const meta = await takeSnapshot(cam.id)
      showToast(`Snapshot captured (${(meta.size_bytes / 1024).toFixed(0)} KB)`, "success")
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      showToast(`Snapshot failed: ${msg}`, "error")
    }
  }

  const onToggleRecord = async (cam: Camera) => {
    if (status?.mode === "connected") {
      showToast(
        "Recording is managed by Command Center in Connected mode — change it there.",
        "info",
      )
      return
    }
    const next = !cam.recording
    setPendingRecord((prev) => {
      const s = new Set(prev)
      s.add(cam.id)
      return s
    })
    // Optimistic — flip locally; server sync will overwrite on next poll.
    setCameras((prev) => prev.map((c) => (c.id === cam.id ? { ...c, recording: next } : c)))
    try {
      await setRecording(cam.id, next)
      showToast(next ? `Recording started — ${cam.name}` : `Recording stopped — ${cam.name}`, "success")
    } catch (e) {
      // Roll back optimistic flip.
      setCameras((prev) => prev.map((c) => (c.id === cam.id ? { ...c, recording: !next } : c)))
      const msg = e instanceof Error ? e.message : String(e)
      showToast(`Recording toggle failed: ${msg}`, "error")
    } finally {
      setPendingRecord((prev) => {
        const s = new Set(prev)
        s.delete(cam.id)
        return s
      })
    }
  }

  if (loading && cameras.length === 0) {
    return (
      <div className="empty-state">
        <div className="spinner" />
        <p style={{ marginTop: "1rem" }}>Loading cameras…</p>
      </div>
    )
  }

  if (error && cameras.length === 0) {
    return (
      <div className="empty-state">
        <h2>Couldn&apos;t load cameras</h2>
        <p>{error}</p>
      </div>
    )
  }

  if (cameras.length === 0) {
    return (
      <div className="empty-state">
        <h2>No cameras detected yet</h2>
        <p>
          Plug in a USB camera (or restart the node after connecting one) and refresh.
          The TUI status bar shows detection progress.
        </p>
      </div>
    )
  }

  const isConnected = status?.mode === "connected"

  return (
    <div className="cameras-grid">
      {cameras.map((cam) => {
        const isDown =
          cam.suspended ||
          cam.status === "offline" ||
          cam.status === "failed" ||
          cam.status === "error"
        const recordPending = pendingRecord.has(cam.id)
        return (
          <div className="camera-card" key={cam.id}>
            <div className="camera-card-header">
              <span className="camera-name" title={cam.id}>
                {cam.name || cam.id}
              </span>
              <span className={`camera-status-pill ${cam.status}`}>{cam.status}</span>
            </div>
            {isDown ? (
              <div className="camera-feed-down">
                <span aria-hidden style={{ fontSize: "1.4rem" }}>⚠</span>
                <span>{cam.suspended ? "Suspended" : cam.last_error ?? cam.status}</span>
              </div>
            ) : isConnected ? (
              // Connected mode: live HLS is duplicated by Command Center
              // (which is the canonical viewer + the source of truth for
              // motion alerts, AI Sentinel, multi-node grids, etc.).
              // Surface a clean redirect instead of a redundant player —
              // the local Snapshots and Recordings tabs are still the
              // only way to see what's archived on this node.
              <div className="camera-feed-cc">
                <div className="camera-feed-cc-title">Live view in Command Center</div>
                <div className="camera-feed-cc-body">
                  This node streams to Command Center for live viewing.
                  Snapshots and recordings captured here still archive locally.
                </div>
                <a
                  href={COMMAND_CENTER_URL}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="btn btn-primary"
                >
                  Open Command Center →
                </a>
              </div>
            ) : (
              <HlsPlayer src={cam.hls_url} className="camera-feed" />
            )}
            <div className="camera-controls">
              <button
                className="btn"
                onClick={() => void onSnapshot(cam)}
                disabled={isDown}
                title="Capture a snapshot from the most recent live segment"
              >
                Snapshot
              </button>
              {/* Hide the Record toggle in Connected mode — Command
                  Center owns recording policy there.  The button used
                  to render disabled with a tooltip, but that clutters
                  the tile without offering any action.  The 409 from
                  the endpoint stays as defence-in-depth. */}
              {!isConnected && (
                <button
                  className={`btn ${cam.recording ? "btn-record-active" : ""}`}
                  onClick={() => void onToggleRecord(cam)}
                  disabled={isDown || recordPending}
                  title={cam.recording ? "Stop recording" : "Start recording"}
                >
                  <span className="record-dot" />
                  {recordPending ? "…" : cam.recording ? "Recording" : "Record"}
                </button>
              )}
            </div>
          </div>
        )
      })}
    </div>
  )
}
