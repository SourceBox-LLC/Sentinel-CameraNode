// Storage — how much this node keeps before deleting its oldest
// recordings.  Backed by GET/PUT /api/storage.  The cap used to be set
// only by the setup wizard; changing it here is saved and applies at
// once, and a lower cap deletes the oldest recordings straight away.

import { FormEvent, useEffect, useState } from "react"

import { getStorage, setStorageCap, StorageInfo } from "../lib/api"
import { useToasts } from "../lib/toasts"

const GIB = 1024 * 1024 * 1024

function gb(bytes: number): string {
  const v = bytes / GIB
  return v >= 10 ? v.toFixed(0) : v.toFixed(1)
}

export default function StoragePage() {
  const { showToast } = useToasts()
  const [info, setInfo] = useState<StorageInfo | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [value, setValue] = useState("")
  const [saving, setSaving] = useState(false)
  const [formError, setFormError] = useState<string | null>(null)

  useEffect(() => {
    getStorage()
      .then((s) => {
        setInfo(s)
        setValue(String(s.max_size_gb))
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
  }, [])

  if (error) {
    return (
      <div className="empty-state">
        <h2>Couldn&apos;t load storage</h2>
        <p>{error}</p>
      </div>
    )
  }
  if (!info) {
    return (
      <div className="empty-state">
        <div className="spinner" />
      </div>
    )
  }

  const capBytes = info.max_size_gb * GIB
  const percent = capBytes > 0 ? Math.min(100, (info.used_bytes / capBytes) * 100) : 0
  const requested = Number(value)
  const valid = Number.isInteger(requested) && requested >= info.min_size_gb
  const lowering = valid && requested < info.max_size_gb
  const wouldDelete = lowering && info.used_bytes > requested * GIB

  const onSubmit = async (e: FormEvent) => {
    e.preventDefault()
    if (!valid) return
    setSaving(true)
    setFormError(null)
    try {
      const next = await setStorageCap(requested)
      setInfo(next)
      setValue(String(next.max_size_gb))
      const freed = next.freed_bytes ?? 0
      showToast(
        freed > 0
          ? `Storage cap set to ${next.max_size_gb} GB. Deleted ${gb(freed)} GB of the oldest recordings.`
          : `Storage cap set to ${next.max_size_gb} GB.`,
        "success",
      )
    } catch (err) {
      setFormError(err instanceof Error ? err.message : String(err))
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="storage-page">
      <section className="storage-card" aria-labelledby="storage-usage">
        <h2 id="storage-usage">Recordings on this node</h2>
        <div
          className="storage-bar"
          role="meter"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.round(percent)}
          aria-label="Storage used"
        >
          <div className={`storage-bar-fill${percent >= 90 ? " high" : ""}`} style={{ width: `${percent}%` }} />
        </div>
        <p className="storage-figures">
          <strong>{gb(info.used_bytes)} GB</strong> used of a{" "}
          <strong>{info.max_size_gb} GB</strong> cap
          {info.disk_total_bytes > 0 && (
            <>
              {" "}· disk: {gb(info.disk_free_bytes)} GB free of {gb(info.disk_total_bytes)} GB
            </>
          )}
        </p>
      </section>

      <form className="storage-card" onSubmit={onSubmit} aria-labelledby="storage-cap">
        <h2 id="storage-cap">Storage cap</h2>
        <p className="storage-help">
          When recordings and snapshots pass the cap, the oldest are deleted to make room.
          The node checks every 5 minutes.
        </p>
        <label className="storage-field">
          <span>Keep up to</span>
          <input
            type="number"
            inputMode="numeric"
            min={info.min_size_gb}
            step={1}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            disabled={saving}
          />
          <span>GB</span>
        </label>
        {wouldDelete && (
          <p className="storage-warning" role="note">
            This node holds {gb(info.used_bytes)} GB. Saving deletes about{" "}
            {gb(info.used_bytes - requested * GIB)} GB of the oldest recordings now.
          </p>
        )}
        {formError && (
          <p className="storage-error" role="alert">
            {formError}
          </p>
        )}
        <button
          type="submit"
          className="btn btn-primary"
          disabled={saving || !valid || requested === info.max_size_gb}
        >
          {saving ? "Saving…" : "Save"}
        </button>
      </form>
    </div>
  )
}
