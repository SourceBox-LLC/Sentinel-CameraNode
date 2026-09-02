// Top-level shell: brand + nav + mode pill, with a child <Outlet/>
// for whichever page is routed.  Status is fetched once on mount
// (mode + node_id) — pages refresh their own data.

import { useEffect, useState } from "react"
import { NavLink, Outlet } from "react-router-dom"

import { COMMAND_CENTER_URL_FALLBACK, getStatus, logout, NodeStatus, refreshSession } from "./lib/api"
import { ToastProvider } from "./lib/toasts"

// Well under the 30-day session lifetime (src/server/auth.rs's
// SESSION_LIFETIME_SECS) — the goal is "an open tab never actually
// hits the wall," not tight timing, so a wide margin costs nothing.
const SESSION_REFRESH_INTERVAL_MS = 24 * 3600 * 1000

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

  // Keep the session alive across a long-open tab (this is a
  // security-camera dashboard plausibly left open on a wall-mounted
  // display). Only runs once requires_auth is confirmed true — a
  // loopback-only node has no session to refresh, and calling this
  // before status has loaded once would be a wasted 401 round-trip.
  // The cookie is HttpOnly so there's no way to check "is it close to
  // expiry" client-side (see refreshSession's doc comment) — this just
  // calls it periodically on a wide margin instead. If the session
  // already expired for some other reason, the call itself 401s and
  // jsonFetch's global handler redirects to /login, same as any other
  // request would.
  useEffect(() => {
    if (!status?.requires_auth) return undefined
    const id = setInterval(() => {
      refreshSession().catch(() => {
        // Network blip or an actual 401 (handled by jsonFetch's global
        // redirect already firing) — nothing further to do here.
      })
    }, SESSION_REFRESH_INTERVAL_MS)
    return () => clearInterval(id)
  }, [status?.requires_auth])

  const mode = status?.mode ?? "local"
  const nodeIdShort = status?.node_id?.slice(0, 8) ?? "—"

  return (
    <ToastProvider>
      <div className="app-shell">
        <header className="app-header">
          <div className="app-brand">
            <div className="app-brand-mark" aria-hidden />
            <div>
              <div className="app-brand-text">Sentinel</div>
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
          {/* Only a node reachable beyond localhost has a session to
              log out of — see NodeStatus.requires_auth. */}
          {status?.requires_auth && (
            <button
              type="button"
              className="btn app-logout-btn"
              onClick={async () => {
                try {
                  await logout()
                } finally {
                  window.location.assign("/login")
                }
              }}
            >
              Log out
            </button>
          )}
        </header>
        <Outlet context={status} />
        {/* Local-mode upsell footer.  Only renders when this node hasn't
            been paired with a Command Center org — connected installs
            already have the full management surface and don't need the
            CTA.  Keep it tasteful: describe the capabilities, no social
            proof / "X cameras online" claims (we're pre-PMF). */}
        {mode === "local" && (
          <LocalUpsell url={status?.command_center_url ?? COMMAND_CENTER_URL_FALLBACK} />
        )}
      </div>
    </ToastProvider>
  )
}

function LocalUpsell({ url }: { url: string }) {
  return (
    <footer className="local-upsell">
      <div className="local-upsell-content">
        <div className="local-upsell-title">Get more out of your cameras</div>
        <p className="local-upsell-body">
          Connect this node to{" "}
          <strong>Sentinel Command Center</strong> for access from
          anywhere, motion-event email alerts, multi-node dashboards, and AI
          assistants that can see what your cameras see — all without losing
          your local-only setup.
        </p>
      </div>
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        className="btn btn-primary local-upsell-cta"
      >
        Explore Command Center →
      </a>
    </footer>
  )
}
