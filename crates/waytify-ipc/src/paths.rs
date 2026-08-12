//! XDG paths, resolved in one place so the daemon and its clients cannot disagree
//! about where the socket lives.

use std::path::PathBuf;

/// Directory name used under each XDG root.
pub const APP: &str = "waytify";

/// Runtime directory, where the socket goes.
///
/// `XDG_RUNTIME_DIR` is set by systemd on any normal login session. The `/tmp`
/// fallback exists for odd environments such as a bare TTY without a session, and
/// is namespaced by user so two accounts cannot collide.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join(APP);
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/{APP}-{user}"))
}

/// The Unix socket every client connects to.
///
/// Override with `WAYTIFY_SOCKET` to run a second daemon side by side, which is
/// how the test suite avoids touching a live one.
pub fn socket_path() -> PathBuf {
    resolve_socket(std::env::var_os("WAYTIFY_SOCKET"))
}

/// The socket path for a given value of the override.
///
/// Split out so it can be tested both ways. Reading the environment inside the
/// test would make the result depend on the shell it runs in, and a developer
/// running a second daemon has that variable set, which is the situation the
/// override exists for in the first place.
fn resolve_socket(override_path: Option<std::ffi::OsString>) -> PathBuf {
    match override_path {
        Some(p) => PathBuf::from(p),
        None => runtime_dir().join("sock"),
    }
}

/// Config directory, holding `config.toml` and `style.css`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join(APP);
    }
    home().join(".config").join(APP)
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// User stylesheet for the popup. Watched for changes and reloaded in place.
pub fn style_file() -> PathBuf {
    config_dir().join("style.css")
}

/// Cache directory, holding downloaded album art and lyrics.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join(APP);
    }
    home().join(".cache").join(APP)
}

/// Album art, keyed by track id. Safe to delete at any time.
pub fn art_cache_dir() -> PathBuf {
    cache_dir().join("art")
}

/// Lyrics, keyed by track id.
pub fn lyrics_cache_dir() -> PathBuf {
    cache_dir().join("lyrics")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_sits_under_the_runtime_dir() {
        // The shape rather than an absolute path, which would depend on the
        // machine.
        let sock = resolve_socket(None);
        assert_eq!(sock.file_name().unwrap(), "sock");
        assert_eq!(sock.parent(), Some(runtime_dir().as_path()));
    }

    #[test]
    fn the_socket_override_is_taken_exactly_as_given() {
        // Not joined onto the runtime directory and not given a suffix: a second
        // daemon is started by pointing this at a path, and anything added to it
        // would send its clients somewhere else.
        let sock = resolve_socket(Some("/tmp/somewhere/else.sock".into()));
        assert_eq!(sock, PathBuf::from("/tmp/somewhere/else.sock"));
    }

    #[test]
    fn caches_are_siblings_under_one_root() {
        // Everything cached must live under a single directory so that clearing
        // the cache is one `rm -rf`, not a scavenger hunt.
        let root = cache_dir();
        assert!(art_cache_dir().starts_with(&root));
        assert!(lyrics_cache_dir().starts_with(&root));
    }

    #[test]
    fn config_and_style_share_a_directory() {
        assert_eq!(config_file().parent(), style_file().parent());
    }
}
