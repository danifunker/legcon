//! The AppKit shell: a pure view over `legcon::controller` (docs/PLAN.md D44).
//!
//! Layers and rules: CONTRIBUTING.md § Architecture policy. Nothing here names the model or a
//! protocol crate; `scripts/check-boundaries.sh` enforces that. Screen for screen it is the Win32
//! shell (frontends/win32), drawn with AppKit controls.

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod dialog;
#[cfg(target_os = "macos")]
mod effects;
#[cfg(target_os = "macos")]
mod form;
#[cfg(target_os = "macos")]
mod header;
#[cfg(target_os = "macos")]
mod table;
#[cfg(target_os = "macos")]
mod tabstrip;
#[cfg(target_os = "macos")]
mod term_view;
#[cfg(target_os = "macos")]
mod ui;
#[cfg(target_os = "macos")]
mod wake;
#[cfg(target_os = "macos")]
mod window;

#[cfg(target_os = "macos")]
fn main() {
    legcon::controller::log::capture::init();
    app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("legcon's AppKit shell runs on macOS only; use the lc CLI.");
}
