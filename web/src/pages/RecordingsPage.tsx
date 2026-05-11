// Recordings index — one cell per (camera, date) tuple from
// /api/recordings.  Click a cell → open the playback modal pointed
// at the dynamic VOD HLS playlist for that bucket.

import { useEffect, useState } from "react"

import HlsPlayer from "../components/HlsPlayer"
import { listRecordings, recordingPlaylistUrl, RecordingSummary } from "../lib/api"

export default function RecordingsPage() {
  const [recs, setRecs] = useState<RecordingSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [playing, setPlaying] = useState<RecordingSummary | null>(null)

  // Poll every 10s.  Recording buckets change when a new (camera,
  // date) tuple starts archiving — toggled by the operator in CC
  // (Connected) or in the local SPA (Local).  Without polling the
  // tab stays stale until manual refresh.  Matches the Snapshots tab
  // cadence.
  useEffect(() => {
    let cancelled = false
    const load = async () => {
      try {
        const list = await listRecordings()
        if (!cancelled) {
          setRecs(list)
          setError(null)
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      } finally {
        if (!cancelled) setLoading(false)
      }
    }
    void load()
    const id = setInterval(load, 10_000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  if (loading) {
    return (
      <div className="empty-state">
        <div className="spinner" />
        <p style={{ marginTop: "1rem" }}>Loading recordings…</p>
      </div>
    )
  }

  if (error) {
    return (
      <div className="empty-state">
        <h2>Couldn&apos;t load recordings</h2>
        <p>{error}</p>
      </div>
    )
  }

  if (recs.length === 0) {
    return (
      <div className="empty-state">
        <h2>No recordings yet</h2>
        <p>
          Toggle <strong>Record</strong> on a camera tile to start archiving
          segments to local storage.  In Connected mode, change the recording
          policy in the Command Center UI.
        </p>
      </div>
    )
  }

  return (
    <>
      <div className="recordings-list">
        {recs.map((r) => (
          <button
            key={`${r.camera_id}-${r.date}`}
            className="recording-cell"
            onClick={() => setPlaying(r)}
          >
            <div className="recording-cell-camera">{r.camera_id}</div>
            <div className="recording-cell-date">{r.date}</div>
            <div className="recording-cell-meta">
              {r.segment_count.toLocaleString()} segments ·{" "}
              {(r.total_size_bytes / (1024 * 1024)).toFixed(1)} MB
            </div>
          </button>
        ))}
      </div>
      {playing && (
        <div className="player-modal-backdrop" onClick={() => setPlaying(null)}>
          <div className="player-modal" onClick={(e) => e.stopPropagation()}>
            <div className="player-modal-header">
              <div>
                <div className="player-modal-title">{playing.camera_id}</div>
                <div style={{ color: "var(--text-secondary)", fontSize: "0.85rem" }}>
                  {playing.date} · {playing.segment_count.toLocaleString()} segments
                </div>
              </div>
              <button
                className="player-modal-close"
                onClick={() => setPlaying(null)}
                aria-label="Close player"
              >
                ×
              </button>
            </div>
            <HlsPlayer
              src={recordingPlaylistUrl(playing.camera_id, playing.date)}
              autoPlay
              controls
              muted={false}
            />
          </div>
        </div>
      )}
    </>
  )
}
