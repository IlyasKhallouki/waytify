//! Stylesheet loading, in three layers.
//!
//! GTK resolves providers by priority, which lets a user stylesheet override the
//! defaults without having to restate them. The order, lowest first:
//!
//! 1. Defaults compiled into the binary, so a fresh install looks finished.
//! 2. Colours derived from the current album art, regenerated per track. Themes
//!    opt in by referencing `@art_vibrant`; a theme that ignores them is
//!    unaffected, which is why this can sit above the defaults safely.
//! 3. `~/.config/waytify/style.css`, watched and reloaded in place.
//!
//! Reloading swaps the provider's contents rather than adding another one.
//! Adding would leave every previous version still in the cascade, so a rule the
//! user deleted would go on applying until restart.

use gtk4::prelude::*;
use gtk4::{CssProvider, glib};
use waytify_ipc::{ArtColors, paths};

/// Baked-in defaults.
const DEFAULT_CSS: &str = include_str!("default.css");

/// The baked stylesheet, for tests that check it still parses.
#[cfg(test)]
pub(crate) fn default_css() -> &'static str {
    DEFAULT_CSS
}

const PRIORITY_DEFAULT: u32 = gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION;
/// Above the defaults so art colours can replace them, below the user stylesheet
/// so the user always wins.
const PRIORITY_ART: u32 = gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1;
const PRIORITY_USER: u32 = gtk4::STYLE_PROVIDER_PRIORITY_USER;

thread_local! {
    /// Kept alive for the process lifetime and reused on every reload. Dropping a
    /// provider would remove its rules; replacing its contents is what makes
    /// editing the stylesheet feel live.
    static ART: CssProvider = CssProvider::new();
    static USER: CssProvider = CssProvider::new();
}

pub fn install(window: &gtk4::ApplicationWindow) {
    let display = WidgetExt::display(window);

    let defaults = CssProvider::new();
    defaults.load_from_string(DEFAULT_CSS);
    gtk4::style_context_add_provider_for_display(&display, &defaults, PRIORITY_DEFAULT);
    // Leaked deliberately: these must outlive this function for the whole process
    // and there is exactly one of each.
    std::mem::forget(defaults);

    ART.with(|p| gtk4::style_context_add_provider_for_display(&display, p, PRIORITY_ART));
    USER.with(|p| gtk4::style_context_add_provider_for_display(&display, p, PRIORITY_USER));

    load_user_stylesheet();
    watch_user_stylesheet();
}

/// Load the user stylesheet, or clear it if the file has gone.
fn load_user_stylesheet() {
    let path = paths::style_file();
    let css = std::fs::read_to_string(&path).unwrap_or_default();
    if css.is_empty() {
        tracing::debug!("no user stylesheet at {}", path.display());
    }
    // GTK reports parse errors on its own logging channel rather than returning
    // them, so a broken stylesheet shows up in the daemon's stderr rather than
    // silently doing nothing.
    USER.with(|p| p.load_from_string(&css));
}

/// Watch the config directory and reload when the stylesheet changes.
///
/// The directory rather than the file: editors overwrite by writing a temporary
/// file and renaming it over the target, which replaces the inode and leaves a
/// watch on the old file pointing at nothing.
fn watch_user_stylesheet() {
    use notify::{RecursiveMode, Watcher};

    let dir = paths::config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let (tx, rx) = async_channel::unbounded::<()>();
    let target = paths::style_file();

    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if event.paths.contains(&target) {
            let _ = tx.send_blocking(());
        }
    });

    let Ok(mut watcher) = watcher else {
        tracing::debug!("could not watch {} for changes", dir.display());
        return;
    };
    if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
        return;
    }
    // The watcher stops when dropped, and nothing else owns it.
    std::mem::forget(watcher);

    glib::spawn_future_local(async move {
        while rx.recv().await.is_ok() {
            // Editors often produce several events for one save. Collapsing them
            // avoids parsing the file three times for one keystroke.
            glib::timeout_future(std::time::Duration::from_millis(60)).await;
            while rx.try_recv().is_ok() {}
            tracing::debug!("reloading the user stylesheet");
            load_user_stylesheet();
        }
    });
}

/// Publish the current track's colours as named GTK colours.
///
/// Called on every track change. A theme referencing `@art_vibrant` follows the
/// record; one that does not is unaffected, which is why this is safe to apply
/// unconditionally.
pub fn set_art_colors(colors: Option<ArtColors>) {
    let css = match colors {
        Some(c) => format!(
            "@define-color art_vibrant {};\n\
             @define-color art_muted {};\n\
             @define-color art_on_vibrant {};\n",
            c.vibrant.to_hex(),
            c.muted.to_hex(),
            c.on_vibrant.to_hex(),
        ),
        // Defined but neutral rather than undefined. An undefined colour is a
        // parse error in GTK, which would break the whole user stylesheet rather
        // than just this rule.
        None => "@define-color art_vibrant @theme_bg_color;\n\
                 @define-color art_muted @theme_bg_color;\n\
                 @define-color art_on_vibrant @theme_fg_color;\n"
            .to_string(),
    };
    ART.with(|p| p.load_from_string(&css));
}
