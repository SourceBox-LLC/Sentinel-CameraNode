// Sentinel CloudNode local web UI — React entry point.
// Embedded into the Rust binary via rust-embed; served by the warp
// HTTP server at /.  Don't share code with the Command Center
// frontend — keep this self-contained.

import React from "react"
import ReactDOM from "react-dom/client"
import { BrowserRouter, Routes, Route } from "react-router-dom"

import App from "./App"
import CamerasPage from "./pages/CamerasPage"
import LoginPage from "./pages/LoginPage"
import RecordingsPage from "./pages/RecordingsPage"
import SnapshotsPage from "./pages/SnapshotsPage"
import "./styles.css"

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <BrowserRouter>
      <Routes>
        {/* Outside <App/> and never behind a client-side auth check —
            /login itself must always render even with no session, and
            lib/api.ts's jsonFetch is what actually enforces auth (a
            401 from any API call bounces here via a full navigation).
            The real gate is server-side (server::auth's guard); this
            route existing unauthenticated is not a bypass of anything. */}
        <Route path="/login" element={<LoginPage />} />
        <Route path="/" element={<App />}>
          <Route index element={<CamerasPage />} />
          <Route path="snapshots" element={<SnapshotsPage />} />
          <Route path="recordings" element={<RecordingsPage />} />
          {/* SPA-fallback catch-all: warp's static_routes serves
              index.html for unknown paths, React Router handles
              client-side routing.  Anything genuinely unrouteable
              renders the cameras page (the most-useful default). */}
          <Route path="*" element={<CamerasPage />} />
        </Route>
      </Routes>
    </BrowserRouter>
  </React.StrictMode>,
)
