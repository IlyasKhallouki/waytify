//! The daemon connection, and the boundary between two main loops.
//!
//! GTK owns the thread it was initialised on and runs glib's loop there. tokio
//! wants to own a loop too. Rather than trying to marry them, the socket lives on
//! its own thread with its own tokio runtime, and the two sides talk over
//! channels that both loops can wait on. `async-channel` is used because glib can
//! await it directly via [`gtk4::glib::spawn_future_local`].
//!
//! Everything crossing the boundary is owned data, so no GTK type is ever touched
//! off the main thread.

use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use waytify_ipc::{Command, Frame, PROTOCOL_VERSION, PopupAction, Scope, State, paths};

/// Something the window needs to react to.
#[derive(Debug)]
pub enum Update {
    /// A new state to render.
    State(Box<State>),
    /// Show, hide, or toggle.
    Popup(PopupAction),
    /// The daemon went away. The window stays up showing what it last had,
    /// because a player window that vanishes when a background service restarts
    /// would be worse than a slightly stale one.
    Disconnected,
    /// The daemon speaks a protocol this build does not.
    Incompatible(String),
}

pub struct Client {
    pub updates: async_channel::Receiver<Update>,
    commands: async_channel::Sender<Command>,
}

impl Client {
    /// Queue a command for the daemon.
    ///
    /// Deliberately not async and never blocking: this is called from button
    /// handlers on the GTK thread, and stalling there would freeze the window.
    /// An unbounded queue means a command is never dropped for backpressure,
    /// which matters because these are user actions rather than telemetry.
    pub fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            tracing::warn!("command channel closed; the daemon connection is gone");
        }
    }
}

/// Start the connection thread.
///
/// Reconnects on its own, so the window survives a daemon restart without the
/// user noticing anything beyond a moment of stale data.
pub fn connect() -> Client {
    let (update_tx, update_rx) = async_channel::unbounded();
    let (command_tx, command_rx) = async_channel::unbounded();

    std::thread::Builder::new()
        .name("waytify-socket".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => return tracing::error!("could not start the socket runtime: {e}"),
            };
            runtime.block_on(pump(update_tx, command_rx));
        })
        .expect("spawning the socket thread");

    Client { updates: update_rx, commands: command_tx }
}

async fn pump(updates: async_channel::Sender<Update>, commands: async_channel::Receiver<Command>) {
    let socket = paths::socket_path();

    loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => {
                if let Err(e) = session(stream, &updates, &commands).await {
                    tracing::debug!("daemon session ended: {e:#}");
                }
                if updates.send(Update::Disconnected).await.is_err() {
                    return;
                }
            }
            Err(e) => tracing::debug!("cannot reach the daemon: {e}"),
        }

        // Short and flat. Unlike the bar, the popup is only running because
        // someone is looking at it, so a long backoff would be visible.
        tokio::time::sleep(RECONNECT).await;
        if updates.is_closed() {
            return;
        }
    }
}

const RECONNECT: Duration = Duration::from_millis(500);

async fn session(
    stream: UnixStream,
    updates: &async_channel::Sender<Update>,
    commands: &async_channel::Receiver<Command>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    writer.write_all(Command::Subscribe { scope: Scope::Full }.to_line()?.as_bytes()).await?;
    writer.flush().await?;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                match serde_json::from_str::<Frame>(&line) {
                    Ok(Frame::State { state }) => {
                        if updates.send(Update::State(state)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Ok(Frame::Popup { action }) => {
                        if updates.send(Update::Popup(action)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Ok(Frame::Hello { protocol, version }) if protocol != PROTOCOL_VERSION => {
                        let message = format!(
                            "daemon speaks protocol {protocol} (waytify {version}), \
                             this window speaks {PROTOCOL_VERSION}"
                        );
                        let _ = updates.send(Update::Incompatible(message)).await;
                        return Ok(());
                    }
                    Ok(Frame::Error { message }) => tracing::warn!("daemon: {message}"),
                    Ok(Frame::Hello { .. } | Frame::Bar { .. } | Frame::Ack) => {}
                    Err(e) => tracing::warn!("unreadable frame: {e}"),
                }
            }
            command = commands.recv() => {
                let Ok(command) = command else { return Ok(()) };
                writer.write_all(command.to_line()?.as_bytes()).await?;
                writer.flush().await?;
            }
        }
    }
}
