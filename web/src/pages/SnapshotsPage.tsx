// Snapshots gallery — image grid backed by /api/snapshots, with a
// click-to-zoom modal and a per-tile delete action.  Mirrors the
// dark-card aesthetic of the recordings page.

import { useCallback, useEffect, useState } from "react"

import {
  deleteSnapshot,
  listSnapshots,
  SnapshotRecord,
  snapshotImageUrl,
} from "../lib/api"
import { useToasts } from "../lib/toasts"

function formatTimestamp(ms: number): string {
  // Snapshot timestamps come from the node clock as Unix-ms.  Render
  // them in the user's local TZ so the gallery groups by the day the
  // operator actually saw — UTC dates would feel off-by-hours for
  // anyone outside +0.
  const d = new Date(ms)
  return d.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  })
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

export default function SnapshotsPage() {
  const { showToast } = useToasts()
  const [snaps, setSnaps] = useState<SnapshotRecord[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [zoomed, setZoomed] = useState<SnapshotRecord | null>(null)
  const [deleting, setDeleting] = useState<Set<number>>(new Set())

  const refresh = useCallback(async () => {
    try {
      const list = await listSnapshots()
      setSnaps(list)
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const onDelete = async (snap: SnapshotRecord) => {
    // Confirm so an accidental click on the trash icon doesn't drop a
    // capture forever.  No undo path yet; revisit if users ask.
    if (!window.confirm(`Delete snapshot ${snap.filename}?`)) return
    setDeleting((prev) => {
      const s = new Set(prev)
      s.add(snap.id)
      return s
    })
    try {
      await deleteSnapshot(snap.id)
      setSnaps((prev) => prev.filter((s) => s.id !== snap.id))
      // Close the zoom modal if we just deleted the snapshot it was showing.
      if (zoomed?.id === snap.id) setZoomed(null)
      showToast(`Snapshot deleted`, "success")
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      showToast(`Delete failed: ${msg}`, "error")
    } finally {
      setDeleting((prev) => {
        const s = new Set(prev)
        s.delete(snap.id)
        return s
      })
    }
  }

  if (loading) {
    return (
      <div className="empty-state">
        <div className="spinner" />
        <p style={{ marginTop: "1rem" }}>Loading snapshots…</p>
      </div>
    )
  }

  if (error) {
    return (
      <div className="empty-state">
        <h2>Couldn&apos;t load snapshots</h2>
        <p>{error}</p>
      </div>
    )
  }

  if (snaps.length === 0) {
    return (
      <div className="empty-state">
        <h2>No snapshots yet</h2>
        <p>
          Click the <strong>Snapshot</strong> button on a camera tile to capture
          a still.  Snapshots are saved to local encrypted storage and listed
          here.
        </p>
      </div>
    )
  }

  return (
    <>
      <div className="snapshots-grid">
        {snaps.map((snap) => {
          const isDeleting = deleting.has(snap.id)
          return (
            <div className="snapshot-cell" key={snap.id}>
              <button
                type="button"
                className="snapshot-thumb"
                onClick={() => setZoomed(snap)}
                title={`${snap.camera_id} · ${formatTimestamp(snap.timestamp)}`}
              >
                <img
                  src={snapshotImageUrl(snap.id)}
                  alt={`Snapshot from ${snap.camera_id} at ${formatTimestamp(snap.timestamp)}`}
                  loading="lazy"
                />
              </button>
              <div className="snapshot-meta">
                <div className="snapshot-camera">{snap.camera_id}</div>
                <div className="snapshot-time">{formatTimestamp(snap.timestamp)}</div>
                <div className="snapshot-size">{formatBytes(snap.size_bytes)}</div>
              </div>
              <button
                type="button"
                className="btn snapshot-delete"
                onClick={() => void onDelete(snap)}
                disabled={isDeleting}
                title="Delete snapshot"
              >
                {isDeleting ? "…" : "Delete"}
              </button>
            </div>
          )
        })}
      </div>
      {zoomed && (
        <div className="player-modal-backdrop" onClick={() => setZoomed(null)}>
          <div className="player-modal" onClick={(e) => e.stopPropagation()}>
            <div className="player-modal-header">
              <div>
                <div className="player-modal-title">{zoomed.camera_id}</div>
                <div style={{ color: "var(--text-secondary)", fontSize: "0.85rem" }}>
                  {formatTimestamp(zoomed.timestamp)} · {formatBytes(zoomed.size_bytes)}
                </div>
              </div>
              <button
                className="player-modal-close"
                onClick={() => setZoomed(null)}
                aria-label="Close snapshot"
              >
                ×
              </button>
            </div>
            <img
              src={snapshotImageUrl(zoomed.id)}
              alt={`Snapshot from ${zoomed.camera_id}`}
              style={{
                width: "100%",
                maxHeight: "70vh",
                objectFit: "contain",
                background: "#000",
                borderRadius: 8,
              }}
            />
          </div>
        </div>
      )}
    </>
  )
}
