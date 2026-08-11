//! Asking the compositor where the pointer is.
//!
//! Waybar does not report where a module sits on screen, so when the popup is
//! opened from a bar click the only thing that says which monitor was meant, and
//! roughly where, is the pointer.
//!
//! Only Hyprland is implemented. Every compositor exposes this differently or not
//! at all, so the contract is that an unsupported one returns `None` and the
//! caller falls back to a fixed corner. That keeps waytify usable everywhere and
//! nicer where the information exists.

use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use waytify_ipc::Point;

/// The compositor should answer instantly. If it does not, opening the window
/// matters more than opening it in the right place.
const TIMEOUT: Duration = Duration::from_millis(150);

/// Current pointer position in global logical pixels, if it can be determined.
pub async fn cursor_position() -> Option<Point> {
    let reply = tokio::time::timeout(TIMEOUT, query_hyprland("cursorpos")).await.ok()??;
    parse_cursor_position(&reply)
}

async fn query_hyprland(command: &str) -> Option<String> {
    let mut stream = UnixStream::connect(hyprland_socket()?).await.ok()?;
    stream.write_all(command.as_bytes()).await.ok()?;
    // Hyprland answers and closes, so reading to end is the whole reply.
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.ok()?;
    Some(reply)
}

fn hyprland_socket() -> Option<PathBuf> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(PathBuf::from(runtime).join("hypr").join(signature).join(".socket.sock"))
}

/// Parse Hyprland's `cursorpos` reply, which looks like `1538, 825`.
fn parse_cursor_position(reply: &str) -> Option<Point> {
    let (x, y) = reply.trim().split_once(',')?;
    Some(Point { x: x.trim().parse().ok()?, y: y.trim().parse().ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_reply_shape() {
        assert_eq!(parse_cursor_position("1538, 825"), Some(Point { x: 1538, y: 825 }));
    }

    #[test]
    fn tolerates_whitespace_and_trailing_newlines() {
        assert_eq!(parse_cursor_position("  0,0\n"), Some(Point { x: 0, y: 0 }));
    }

    #[test]
    fn negative_coordinates_are_valid() {
        // A monitor placed left of or above the origin gives negative globals.
        assert_eq!(parse_cursor_position("-1920, -100"), Some(Point { x: -1920, y: -100 }));
    }

    #[test]
    fn anything_unexpected_is_no_answer_rather_than_a_guess() {
        // Better to fall back to a fixed corner than to open the window at 0,0
        // because an error string failed to parse as a number.
        for reply in ["", "unknown request", "1538", "a, b", "1538, "] {
            assert_eq!(parse_cursor_position(reply), None, "{reply:?} should not parse");
        }
    }
}
