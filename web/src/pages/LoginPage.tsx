// Local-admin login — shown whenever a request comes back 401 (see
// lib/api.ts's jsonFetch) or the operator navigates here directly.
// Submitting sets an HttpOnly session cookie server-side; a full page
// navigation to "/" afterward re-mounts the app with the new session.

import { FormEvent, useState } from "react"

import { ApiError, login } from "../lib/api"

export default function LoginPage() {
  const [password, setPassword] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function handleSubmit(e: FormEvent) {
    e.preventDefault()
    setError(null)
    setBusy(true)
    try {
      await login(password)
      window.location.assign("/")
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "Login failed.")
      setBusy(false)
    }
  }

  return (
    <div className="login-page">
      <form className="login-card" onSubmit={handleSubmit}>
        <div className="app-brand-mark" aria-hidden />
        <h1 className="login-title">Sentinel</h1>
        <p className="login-subtitle">Sign in to view this node&rsquo;s dashboard.</p>
        <input
          type="password"
          className="login-input"
          placeholder="Password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoFocus
          required
        />
        {error && <div className="login-error">{error}</div>}
        <button type="submit" className="btn btn-primary login-submit" disabled={busy || !password}>
          {busy ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  )
}
