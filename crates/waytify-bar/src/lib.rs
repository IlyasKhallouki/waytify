//! Clients: the streaming Waybar module, and one-shot commands for keybinds.
//!
//! Neither holds state. Both start the daemon if it is not already running, so
//! the only thing a user has to put in their Waybar config is `waytify bar`.

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use waytify_ipc::{BarOutput, Command, Frame, PROTOCOL_VERSION, Scope, State, paths};

/// Waybar hides a custom module whose text is empty, which is the right thing to
/// show while the daemon is starting or after it goes away.
const BLANK: &str = r#"{"text":"","class":["no-player"]}"#;

/// Upper bound on a one-shot command. Slightly longer than the daemon's own
/// per-command timeout, so its specific error wins over this generic one.
const COMMAND_DEADLINE: Duration = Duration::from_secs(4);

/// Run as a Waybar custom module: stream rendered output to stdout forever.
///
/// Waybar keeps this process alive and reads a line at a time, so there is no
/// interval and no polling. If the daemon restarts underneath, this reconnects
/// rather than dying and leaving a dead module behind until the next bar reload.
pub async fn run_bar() -> Result<()> {
    let mut stdout = tokio::io::stdout();

    loop {
        match stream_once(&mut stdout).await {
            Ok(()) => tracing::debug!("daemon closed the connection"),
            Err(e) => tracing::debug!("bar stream ended: {e:#}"),
        }
        // The daemon is gone or restarting. Clear the module rather than leaving
        // a stale track sitting in the bar.
        write_line(&mut stdout, BLANK).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn stream_once(stdout: &mut tokio::io::Stdout) -> Result<()> {
    let stream = connect().await?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(Command::Subscribe { scope: Scope::Bar }.to_line()?.as_bytes()).await?;
    writer.flush().await?;

    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<Frame>(&line) {
            Ok(Frame::Bar { bar }) => {
                write_line(stdout, &serde_json::to_string(&bar)?).await?;
            }
            Ok(Frame::Hello { protocol, version }) => {
                check_protocol(protocol, &version)?;
            }
            Ok(Frame::Error { message }) => tracing::warn!("daemon: {message}"),
            Ok(Frame::Ack | Frame::State { .. }) => {}
            Err(e) => tracing::warn!("unreadable frame: {e}"),
        }
    }
    Ok(())
}

/// Send one command and exit with its outcome. This is what a keybind or a
/// Waybar click runs.
///
/// It waits for the daemon's verdict rather than firing and forgetting, so
/// `waytify next` with no player running exits non-zero and says so instead of
/// looking like it worked.
pub async fn send(command: Command) -> Result<()> {
    let stream = connect().await?;
    let (reader, mut writer) = stream.into_split();
    writer.write_all(command.to_line()?.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let deadline = tokio::time::sleep(COMMAND_DEADLINE);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => anyhow::bail!("the daemon did not answer in time"),
            line = lines.next_line() => {
                let Some(line) = line? else {
                    anyhow::bail!("the daemon closed the connection without answering");
                };
                match serde_json::from_str::<Frame>(&line) {
                    Ok(Frame::Ack) => return Ok(()),
                    Ok(Frame::Error { message }) => anyhow::bail!("{message}"),
                    // Hello arrives first on every connection, and state frames
                    // are possible if a subscription is somehow in flight.
                    Ok(_) => continue,
                    Err(e) => tracing::warn!("unreadable frame: {e}"),
                }
            }
        }
    }
}

/// Fetch the current state once, for scripting and for debugging.
pub async fn snapshot() -> Result<State> {
    let stream = connect().await?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(Command::Subscribe { scope: Scope::Full }.to_line()?.as_bytes()).await?;
    writer.flush().await?;

    while let Some(line) = lines.next_line().await? {
        if let Ok(Frame::State { state }) = serde_json::from_str::<Frame>(&line) {
            return Ok(*state);
        }
    }
    anyhow::bail!("daemon closed the connection before sending state")
}

/// Render a state the way the bar would, without a daemon. Used by `--dry-run`
/// style checks and by the test suite.
pub fn render_offline(bar: &BarOutput) -> Result<String> {
    Ok(serde_json::to_string(bar)?)
}

fn check_protocol(protocol: u32, version: &str) -> Result<()> {
    anyhow::ensure!(
        protocol == PROTOCOL_VERSION,
        "daemon speaks protocol {protocol} (waytify {version}) but this client speaks \
         {PROTOCOL_VERSION}. Restart the daemon with `waytify restart` after upgrading."
    );
    Ok(())
}

/// Connect, starting the daemon if nothing is listening.
///
/// Retries with a short backoff because a daemon that has just been spawned
/// needs a moment to bind, and the first connect will lose that race.
async fn connect() -> Result<UnixStream> {
    let socket = paths::socket_path();

    if let Ok(stream) = UnixStream::connect(&socket).await {
        return Ok(stream);
    }

    spawn_daemon().context("starting the waytify daemon")?;

    let mut delay = Duration::from_millis(25);
    for _ in 0..6 {
        tokio::time::sleep(delay).await;
        if let Ok(stream) = UnixStream::connect(&socket).await {
            return Ok(stream);
        }
        delay *= 2;
    }

    anyhow::bail!("could not reach a waytify daemon at {}", socket.display())
}

/// Start `waytify daemon` detached from this process.
///
/// The new process group matters: without it the daemon shares Waybar's group
/// and takes the same signals, so reloading the bar would kill the daemon it
/// just started.
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command as Proc, Stdio};

    let exe = std::env::current_exe().context("locating the waytify binary")?;
    Proc::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    Ok(())
}

async fn write_line(stdout: &mut tokio::io::Stdout, line: &str) -> Result<()> {
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    // Waybar reads line by line and will sit on a stale label until the buffer
    // happens to flush, so every line is flushed explicitly.
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protocol_mismatch_is_refused_with_an_actionable_message() {
        let err = check_protocol(PROTOCOL_VERSION + 1, "9.9.9").unwrap_err().to_string();
        assert!(err.contains("restart") || err.contains("Restart"), "{err}");
    }

    #[test]
    fn a_matching_protocol_is_accepted() {
        assert!(check_protocol(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION")).is_ok());
    }

    #[test]
    fn the_blank_payload_is_valid_waybar_json() {
        // Emitted on every disconnect, so a mistake here would be visible as a
        // broken module exactly when the daemon is already having a bad time.
        let parsed: BarOutput = serde_json::from_str(BLANK).unwrap();
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.class, vec!["no-player"]);
    }
}
