// Manual probe of the WSL preflight against the real host.  Not a test —
// results depend on what's actually installed.  Run with:
//
//     cargo run --example wsl_preflight_probe
//
// Useful for eyeballing the output when developing the preflight UI.

use sourcebox_sentry_cameranode::setup::wsl_preflight::{
    is_internal_distro, is_likely_camera, probe_distro, probe_usbipd, probe_wsl,
};

fn main() {
    println!("=== probe_wsl ===");
    let wsl = probe_wsl();
    println!("  installed      : {}", wsl.installed);
    println!("  default distro : {:?}", wsl.default_distro);
    println!("  distros        :");
    for d in &wsl.distros {
        let flag = if is_internal_distro(&d.name) {
            "[internal]"
        } else {
            "[usable]"
        };
        println!(
            "    - {:22} state={:10} ver={}  {}",
            d.name, d.state, d.version, flag
        );
    }
    println!();

    if wsl.installed {
        let probe_target = wsl
            .distros
            .iter()
            .find(|d| !is_internal_distro(&d.name))
            .map(|d| d.name.clone());
        if let Some(distro) = probe_target {
            println!("=== probe_distro({}) ===", distro);
            let ds = probe_distro(&distro);
            println!("  has_ffmpeg : {}", ds.has_ffmpeg);
            println!("  version    : {:?}", ds.ffmpeg_version);
            println!();
        }
    }

    println!("=== probe_usbipd ===");
    let host = probe_usbipd();
    println!("  installed : {}", host.usbipd_installed);
    println!("  version   : {:?}", host.usbipd_version);
    println!("  devices   :");
    for d in &host.devices {
        let cam = if is_likely_camera(&d.name) {
            "[camera]"
        } else {
            ""
        };
        println!(
            "    - busid={:6} vid:pid={:11} name={:50} state={:15} {}",
            d.busid, d.vid_pid, d.name, d.state, cam
        );
    }
}
