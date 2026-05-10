// Typed wrappers for the CloudNode local web API (Phase B).
// Same-origin: every request lands on the warp server that's also
// serving this SPA, so no base URL or CORS dance.

export type CameraStatus =
  | "starting"
  | "streaming"
  | "online"
  | "restarting"
  | "failed"
  | "error"
  | "offline"

export interface Camera {
  id: string
  name: string
  resolution: string
  status: CameraStatus
  last_error: string | null
  video_codec: string
  audio_codec: string
  segments_uploaded: number
  bytes_uploaded: number
  hls_url: string
  suspended: boolean
  recording: boolean
}

export interface SnapshotMeta {
  id: number
  camera_id: string
  filename: string
  timestamp: number
  size_bytes: number
  image_url: string
}

export interface SnapshotRecord {
  id: number
  camera_id: string
  filename: string
  timestamp: number
  size_bytes: number
}

export interface RecordingSummary {
  camera_id: string
  date: string
  segment_count: number
  total_size_bytes: number
}

export type NodeMode = "local" | "connected"

export interface NodeStatus {
  mode: NodeMode
  version: string
  uptime_secs: number
  node_id: string
  camera_count: number
  active_camera_count: number
  total_segments: number
  total_bytes_uploaded: number
  plan: string | null
}

class ApiError extends Error {
  constructor(public status: number, message: string, public code?: string) {
    super(message)
    this.name = "ApiError"
  }
}

async function jsonFetch<T>(input: RequestInfo, init?: RequestInit): Promise<T> {
  const res = await fetch(input, init)
  if (!res.ok) {
    let body: { error?: string; message?: string } | null = null
    try {
      body = (await res.json()) as { error?: string; message?: string }
    } catch {
      // Non-JSON error; fall through.
    }
    const message =
      body?.message ?? body?.error ?? `Request failed with status ${res.status}`
    throw new ApiError(res.status, message, body?.error)
  }
  return (await res.json()) as T
}

export function listCameras(): Promise<Camera[]> {
  return jsonFetch<Camera[]>("/api/cameras")
}

export function takeSnapshot(cameraId: string): Promise<SnapshotMeta> {
  return jsonFetch<SnapshotMeta>(
    `/api/cameras/${encodeURIComponent(cameraId)}/snapshot`,
    { method: "POST" },
  )
}

export function listSnapshots(cameraId?: string): Promise<SnapshotRecord[]> {
  const qs = cameraId ? `?camera_id=${encodeURIComponent(cameraId)}` : ""
  return jsonFetch<SnapshotRecord[]>(`/api/snapshots${qs}`)
}

export function deleteSnapshot(id: number): Promise<{ deleted: number }> {
  return jsonFetch<{ deleted: number }>(`/api/snapshots/${id}`, {
    method: "DELETE",
  })
}

export function snapshotImageUrl(id: number): string {
  return `/api/snapshots/${id}`
}

export function setRecording(
  cameraId: string,
  recording: boolean,
): Promise<{ camera_id: string; recording: boolean }> {
  return jsonFetch(`/api/cameras/${encodeURIComponent(cameraId)}/recording`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ recording }),
  })
}

export function listRecordings(cameraId?: string): Promise<RecordingSummary[]> {
  const qs = cameraId ? `?camera_id=${encodeURIComponent(cameraId)}` : ""
  return jsonFetch<RecordingSummary[]>(`/api/recordings${qs}`)
}

export function recordingPlaylistUrl(cameraId: string, date: string): string {
  return `/api/recordings/${encodeURIComponent(cameraId)}/${encodeURIComponent(
    date,
  )}/playlist.m3u8`
}

export function getStatus(): Promise<NodeStatus> {
  return jsonFetch<NodeStatus>("/api/status")
}

export { ApiError }
