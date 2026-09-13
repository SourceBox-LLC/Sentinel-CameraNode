//! Integration tests for Sentinel CameraNode

use sourcebox_sentry_cameranode::{Config, Result};

#[test]
fn test_config_load_default() -> Result<()> {
    let config = Config::load(None)?;
    assert!(!config.node.name.is_empty());
    assert!(!config.cloud.api_url.is_empty());
    Ok(())
}

#[test]
fn test_camera_detect() {
    // detect_cameras() shells out to FFmpeg on Windows + macOS for
    // device enumeration. v0.1.35 onward CameraNode uses the system
    // FFmpeg (no bundled fallback), so on a test environment without
    // FFmpeg on PATH the call returns Err — that's fine, we're just
    // verifying it doesn't panic.
    match sourcebox_sentry_cameranode::camera::detect_cameras() {
        Ok(cameras) => println!("Detected {} cameras", cameras.len()),
        Err(e) => println!("Camera detect skipped (no FFmpeg on PATH?): {}", e),
    }
}
/// A boxed warp filter must actually bind and answer over TCP.
///
/// The filter tests elsewhere drive routes through `warp::test::request`,
/// which never opens a socket — so nothing covered `warp::serve(...).run()`
/// itself. That mattered during the warp 0.3 -> 0.4 upgrade: the deeply
/// nested filter chain tripped a higher-ranked lifetime error
/// ("implementation of `AsRef` is not general enough") that only appears
/// once the future is spawned onto a runtime, which a `warp::test::request`
/// suite never does. `.boxed()` resolves it; this proves the boxed chain
/// still binds and answers.
///
/// It deliberately does NOT claim to guard the other half of that upgrade.
/// warp 0.4 made every feature opt-in (`default = []`), so dropping
/// `server` from the main dependency leaves the crate with no
/// `warp::serve`. Checked: this test still compiles in that state, because
/// the dev-dependency also declares `server` and Cargo unifies features
/// when building tests. `cargo build` is what catches it, and CI runs that
/// before the tests.
///
/// Port comes from the node's own `find_available_port`, so this cannot
/// collide with a developer's running node.
#[tokio::test]
async fn boxed_filter_binds_and_serves_over_tcp() {
    use warp::Filter;

    let route = warp::path("health")
        .and(warp::get())
        .map(|| "ok")
        .boxed();

    // warp 0.4 dropped bind_ephemeral, so pick a free port with the
    // node's own helper — the same one the real server uses at boot.
    let port = sourcebox_sentry_cameranode::config::find_available_port(18771);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let handle = tokio::spawn(warp::serve(route).run(addr));
    // Give the listener a moment to come up before the request.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let body = reqwest::get(format!("http://{addr}/health"))
        .await
        .expect("request to the bound server failed")
        .text()
        .await
        .expect("response body");

    assert_eq!(body, "ok");
    handle.abort();
}
