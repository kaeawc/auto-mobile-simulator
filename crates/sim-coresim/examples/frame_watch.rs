//! Checks a booted device's frame callbacks (`Display::watch_frames`, the desktop's frame clock):
//! toggles dark mode a few times and prints how many frames the callback announced and whether
//! the surface changed. A runtime whose callback fails prints the error instead.
//!
//! `cargo run -p simee-coresim --example frame_watch -- <udid>`

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use simee_coresim::{Appearance, Context};

    let Some(udid) = std::env::args().nth(1) else {
        return eprintln!("usage: frame_watch <udid>");
    };
    let run = || -> simee_coresim::Result<(u64, u32, u32)> {
        let device = Context::new()?.device(&udid)?;
        let display = device.display()?;
        let frames = Arc::new(AtomicU64::new(0));
        let counter = frames.clone();
        let before = display.seed();
        let watch = display.watch_frames(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        })?;
        for appearance in [Appearance::Dark, Appearance::Light, Appearance::Dark, Appearance::Light] {
            device.set_appearance(appearance)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        drop(watch);
        Ok((frames.load(Ordering::Relaxed), before, display.seed()))
    };
    match run() {
        Ok((frames, before, after)) => println!("frames {frames}, surface seed {before} -> {after}"),
        Err(e) => {
            eprintln!("frame_watch: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
