// Pre-build hook: ensure `web-dist/` exists with at least a
// placeholder `index.html`, so `cargo build` works on a fresh
// checkout where the contributor hasn't yet run `npm run build`
// in `web/`.
//
// The placeholder ships a "Web UI not built" page with the exact
// command to run, plus the same `<div id="root">` mount point the
// real build outputs.  The runtime smoke test
// (`web_assets_includes_index_html` in src/server/api.rs) checks for
// the mount point, so this satisfies it; the page itself shows a
// clear banner so an operator who runs the binary without the npm
// build step doesn't get a silent broken UI.
//
// On a real Phase C build, `npm run build` writes the actual SPA to
// `web-dist/`, overwriting this placeholder.  The placeholder only
// appears when nobody has built the frontend yet.

use std::fs;
use std::path::Path;

const PLACEHOLDER_INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>SourceBox Sentry Node — Web UI not built</title>
<style>
  body {
    background: #0f1115;
    color: #e8eaf0;
    font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
    margin: 0;
    padding: 4rem 1.5rem;
    text-align: center;
  }
  h1 { font-size: 1.4rem; margin-bottom: 1rem; }
  code {
    background: #161922;
    padding: 0.5rem 1rem;
    border-radius: 6px;
    display: inline-block;
    margin: 0.5rem 0;
    font-family: "JetBrains Mono", ui-monospace, Menlo, monospace;
  }
  p { color: #a8aebc; max-width: 540px; margin: 1rem auto; }
</style>
</head>
<body>
<div id="root">
<h1>Web UI not built</h1>
<p>This binary was compiled without the local web UI bundle.  The Rust
server is running and the <code>/api/*</code> endpoints respond, but
the browser dashboard wasn't included.</p>
<p>To fix, run the frontend build then rebuild the binary:</p>
<code>cd web && npm install &amp;&amp; npm run build</code>
<br />
<code>cargo build --release</code>
</div>
</body>
</html>
"#;

fn main() {
    let dir = Path::new("web-dist");
    let index = dir.join("index.html");

    // Re-run when web-dist content changes — keeps the placeholder
    // logic out of the way of real Vite builds.  Cargo only picks
    // this up when something actually changes; no every-build delay.
    println!("cargo:rerun-if-changed=web-dist");

    if !index.exists() {
        if let Err(e) = fs::create_dir_all(dir) {
            // Don't fail the build over a missing dir — rust-embed
            // will produce a clearer compile error if the folder
            // genuinely can't be created.
            println!("cargo:warning=could not create web-dist/: {}", e);
            return;
        }
        if let Err(e) = fs::write(&index, PLACEHOLDER_INDEX) {
            println!("cargo:warning=could not write placeholder index.html: {}", e);
        } else {
            println!(
                "cargo:warning=web-dist/index.html missing — wrote a placeholder. \
                 Run `npm install && npm run build` in `web/` for the real Phase C SPA.",
            );
        }
    }
}
