// Top-level shell: brand + nav + mode pill, with a child <Outlet/>
// for whichever page is routed.  Status is fetched once on mount
// (mode + node_id) — pages refresh their own data.

import { useEffect, useState } from "react"
import { NavLink, Outlet } from "react-router-dom"

import { COMMAND_CENTER_URL, getStatus, NodeStatus } from "./lib/api"
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
              to="/snapshots"
              className={({ isActive }) => (isActive ? "active" : undefined)}
            >
              Snapshots
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
        {/* Local-mode upsell footer.  Only renders when this node hasn't
            been paired with a Command Center org — connected installs
            already have the full management surface and don't need the
            CTA.  Keep it tasteful: describe the capabilities, no social
            proof / "X cameras online" claims (we're pre-PMF). */}
        {mode === "local" && <LocalUpsell />}
      </div>
    </ToastProvider>
  )
}

function LocalUpsell() {
  return (
    <footer className="local-upsell">
      <div className="local-upsell-content">
        <div className="local-upsell-title">Get more out of your cameras</div>
        <p className="local-upsell-body">
          Connect this node to{" "}
          <strong>SourceBox Sentry Command Center</strong> for access from
          anywhere, motion-event email alerts, multi-node dashboards, and AI
          assistants that can see what your cameras see — all without losing
          your local-only setup.
        </p>
      </div>
      <a
        href={COMMAND_CENTER_URL}
        target="_blank"
        rel="noopener noreferrer"
        className="btn btn-primary local-upsell-cta"
      >
        Explore Command Center →
      </a>
    </footer>
  )
}
