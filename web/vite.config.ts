import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"

// Build artifacts land at ../web-dist so the Rust binary's
// rust-embed `#[folder = "web-dist"]` picks them up at compile time.
// `base: "/"` keeps every asset URL absolute — the warp static
// route serves them from a single mount.
export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    outDir: "../web-dist",
    emptyOutDir: true,
    sourcemap: false,
    // Inline small assets so the binary doesn't ship a long tail
    // of tiny files; anything > 4 KB still goes to /assets/.
    assetsInlineLimit: 4096,
  },
  server: {
    port: 5174,
    // dev-mode only: proxy /api and /hls to a running CloudNode so
    // `npm run dev` works against a live backend. CI / production
    // never hits this — vite is build-time only there.
    proxy: {
      "/api": "http://localhost:8080",
      "/hls": "http://localhost:8080",
    },
  },
})
