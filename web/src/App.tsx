// Top-level shell: brand + nav + mode pill, with a child <Outlet/>
// for whichever page is routed.  Status is fetched once on mount
// (mode + node_id) — pages refresh their own data.

import { useEffect, useState } from "react"
import { NavLink, Outlet } from "react-router-dom"

import { getStatus, NodeStatus } from "./lib/api"
import { ToastProvider } from "./lib/toasts"

export default function App() {
  const [status, setStatus] = useState<NodeStatus | null>(null)

  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      try {
        const s = await getStatus()
        if (!cancelled) setStatus(s)
      } catch {
        // Status is decorative — show "—" when unavailable.
      }
    }
    tick()
    const id = setInterval(tick, 30_000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  const mode = status?.mode ?? "local"
  const nodeIdShort = status?.node_id?.slice(0, 8) ?? "—"

  return (
    <ToastProvider>
      <div className="app-shell">
        <header className="app-header">
          <div className="app-brand">
            <div className="app-brand-mark" aria-hidden />
            <div>
              <div className="app-brand-text">SourceBox Sentry</div>
              <span className="app-brand-sub">Node · {nodeIdShort}</span>
            </div>
          </div>
          <nav className="app-nav">
            <NavLink to="/" end className={({ isActive }) => (isActive ? "active" : undefined)}>
              Cameras
            </NavLink>
            <NavLink
              to="/recordings"
              className={({ isActive }) => (isActive ? "active" : undefined)}
            >
              Recordings
            </NavLink>
          </nav>
          <span className={`app-mode-pill ${mode}`}>{mode === "local" ? "Local" : "Connected"}</span>
        </header>
        <Outlet context={status} />
      </div>
    </ToastProvider>
  )
}
