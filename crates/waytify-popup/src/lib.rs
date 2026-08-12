//! The player window: a `gtk4-layer-shell` surface, not a Waybar plugin.
//!
//! Being a layer-shell surface rather than something embedded in a bar is the
//! point. It works under Waybar, under another bar, or under none at all bound to
//! a compositor keybind, because as far as the compositor is concerned it is an
//! ordinary Wayland client.
//!
//! This process holds no state. It renders whatever the daemon last sent and
//! sends commands back, which keeps it cheap to kill and restart.

pub(crate) mod client;
mod style;
mod ui;
pub mod window;

use anyhow::{Context, Result};
use gtk4::prelude::*;
use gtk4::{Application, gio};
use waytify_ipc::Point;

/// GTK application id. Also the Wayland app id, which is what compositor rules
/// key off, so it is worth being stable and predictable.
const APP_ID: &str = "app.waytify.Popup";

/// How the popup was started.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Show immediately rather than waiting to be told. Set when the daemon
    /// spawned this process in response to a toggle, so the window that the user
    /// just asked for does not need a second round trip to appear.
    pub show_on_start: bool,
    /// Where the click that opened it landed, in compositor logical pixels.
    pub at: Option<Point>,
}

pub fn run(options: Options) -> Result<()> {
    // GTK must not parse our argv: the subcommand and flags are ours, and GTK
    // would reject them.
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        if let Err(e) = window::build(app, options) {
            tracing::error!("could not build the popup: {e:#}");
            app.quit();
        }
    });

    let exit = app.run_with_args::<&str>(&[]);
    anyhow::ensure!(exit == gtk4::glib::ExitCode::SUCCESS, "gtk exited with {exit:?}");
    Ok(())
}

/// Fail early and clearly when there is no compositor to attach to.
///
/// Without this the failure surfaces from deep inside GTK as an unhelpful abort,
/// which is a bad first experience for someone running the command over SSH or on
/// an X11 session by mistake.
pub fn check_environment() -> Result<()> {
    std::env::var_os("WAYLAND_DISPLAY")
        .map(|_| ())
        .context("the popup needs a Wayland session; WAYLAND_DISPLAY is not set")
}
