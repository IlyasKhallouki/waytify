//! The daemon: one engine, one socket, many short-lived clients.
//!
//! Clients are disposable by design. Waybar respawns its module process on every
//! config and style reload, so anything expensive to build (D-Bus connections,
//! caches, and later OAuth tokens) has to live here and outlive them.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};
use waytify_core::clock::Attention;
use waytify_core::config::Config;
use waytify_core::engine::{Engine, EngineMsg};
use waytify_core::format::render_bar;
use waytify_ipc::{Command, Frame, PROTOCOL_VERSION, Point, PopupAction, Scope, State, paths};

/// How long to wait for a player to answer a command before giving up on it.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Shared by every client task.
struct Ctx {
    config: Config,
    states: watch::Receiver<Arc<State>>,
    engine: mpsc::Sender<EngineMsg>,
    watchers: Watchers,
    shutdown: tokio::sync::Notify,
    /// Show and hide requests, fanned out to whichever popup process is
    /// connected. Broadcast rather than a single channel because the daemon does
    /// not track which client is the window, only that some Full subscriber is.
    popup: broadcast::Sender<PopupAction>,
}

/// How many clients of each kind are connected, which decides how hard the
/// engine works at keeping the position accurate.
#[derive(Default)]
struct Watchers {
    bar: AtomicUsize,
    full: AtomicUsize,
    /// Full-scope clients whose window is on screen right now.
    watching: AtomicUsize,
}

impl Watchers {
    fn attention(&self) -> Attention {
        // A connected window is not a visible one. It stays subscribed while
        // hidden so that reopening is instant, and polling on its behalf the
        // whole time it sits in the background spends rate limit, and somebody
        // else's bandwidth, on frames nobody can see.
        if self.watching.load(Ordering::Relaxed) > 0 {
            Attention::Popup
        } else if self.bar.load(Ordering::Relaxed) > 0 || self.full.load(Ordering::Relaxed) > 0 {
            Attention::Bar
        } else {
            Attention::Idle
        }
    }

    fn counter(&self, scope: Scope) -> &AtomicUsize {
        match scope {
            Scope::Bar => &self.bar,
            Scope::Full => &self.full,
        }
    }
}

/// Start the daemon and serve until interrupted.
pub async fn run(config: Config) -> Result<()> {
    let socket = paths::socket_path();
    let listener = bind(&socket)?;

    let engine = Engine::new(config.clone()).await.context("starting the MPRIS engine")?;
    let states = engine.subscribe();
    let (engine_tx, engine_rx) = mpsc::channel(64);

    let engine_task = tokio::spawn(async move {
        if let Err(e) = engine.run(engine_rx).await {
            tracing::error!("engine stopped: {e:#}");
        }
    });

    let ctx = Arc::new(Ctx {
        config,
        states,
        engine: engine_tx,
        watchers: Watchers::default(),
        shutdown: tokio::sync::Notify::new(),
        popup: broadcast::channel(8).0,
    });

    tracing::info!("listening on {}", socket.display());
    let result = accept_loop(&listener, &ctx).await;

    // The socket file outlives the process unless it is removed here, and a
    // leftover one makes the next start look like a daemon is already running.
    let _ = std::fs::remove_file(&socket);
    engine_task.abort();
    result
}

async fn accept_loop(listener: &UnixListener, ctx: &Arc<Ctx>) -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting a client")?;
                let ctx = Arc::clone(ctx);
                tokio::spawn(async move {
                    if let Err(e) = serve_client(stream, &ctx).await {
                        tracing::debug!("client disconnected: {e:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted, shutting down");
                return Ok(());
            }
            _ = sigterm.recv() => {
                tracing::info!("terminated, shutting down");
                return Ok(());
            }
            _ = ctx.shutdown.notified() => {
                tracing::info!("shutdown requested by a client");
                return Ok(());
            }
        }
    }
}

/// Bind the socket, refusing to start if another daemon already owns it.
///
/// A socket file left behind by a crash is indistinguishable from a live one on
/// disk. The difference is whether anything answers, so the check is to try
/// connecting: a refused connection means the file is stale and safe to replace.
fn bind(socket: &Path) -> Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        restrict_to_owner(dir);
    }

    if socket.exists() {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(_) => anyhow::bail!("a waytify daemon is already running on {}", socket.display()),
            Err(_) => {
                tracing::debug!("replacing a stale socket at {}", socket.display());
                let _ = std::fs::remove_file(socket);
            }
        }
    }

    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;

    // The runtime directory is already owner-only, so this is a second layer
    // rather than the only one. A socket that accepts playback commands should
    // not be reachable just because a parent directory's mode was loosened.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));

    Ok(listener)
}

/// The runtime directory holds a control socket, so keep it to the owner.
fn restrict_to_owner(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

async fn serve_client(stream: UnixStream, ctx: &Arc<Ctx>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut states = ctx.states.clone();
    let mut popups = ctx.popup.subscribe();
    let mut scope: Option<Scope> = None;
    // Whether this client has said its window is on screen.
    let mut watching = false;

    send(
        &mut writer,
        &Frame::Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    let result = loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break Ok(()) };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Command>(&line) {
                    Ok(Command::Subscribe { scope: requested }) => {
                        set_scope(ctx, &mut scope, requested).await;
                        // Send immediately rather than waiting for the next
                        // change, so a bar starting during a paused track still
                        // renders something.
                        let state = { states.borrow_and_update().clone() };
                        send_state(&mut writer, requested, &state, ctx).await?;
                    }
                    Ok(Command::Watching { active }) => {
                        set_watching(ctx, &mut watching, active).await;
                        send(&mut writer, &Frame::Ack).await?;
                    }
                    Ok(Command::Shutdown) => {
                        // Acknowledge before tearing down, otherwise `waytify stop`
                        // sees the socket close and reports that as a failure.
                        send(&mut writer, &Frame::Ack).await?;
                        ctx.shutdown.notify_waiters();
                        break Ok(());
                    }
                    Ok(cmd) => {
                        // Answered inline rather than in a spawned task, so that
                        // replies keep the order the commands arrived in.
                        let frame = run_command(ctx, cmd).await;
                        send(&mut writer, &frame).await?;
                    }
                    Err(e) => {
                        send(&mut writer, &Frame::Error { message: e.to_string() }).await?;
                    }
                }
            }
            changed = states.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                if let Some(scope) = scope {
                    let state = { states.borrow_and_update().clone() };
                    send_state(&mut writer, scope, &state, ctx).await?;
                }
            }
            action = popups.recv() => {
                match action {
                    Ok(action) if scope == Some(Scope::Full) => {
                        send(&mut writer, &Frame::Popup { action }).await?;
                    }
                    // A client too slow to keep up with window actions has missed
                    // a show or a hide. Nothing useful can be replayed, and the
                    // next request will arrive shortly, so carry on.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
        }
    };

    if watching {
        ctx.watchers.watching.fetch_sub(1, Ordering::Relaxed);
    }
    if let Some(scope) = scope {
        ctx.watchers.counter(scope).fetch_sub(1, Ordering::Relaxed);
    }
    if watching || scope.is_some() {
        notify_attention(ctx).await;
    }
    result
}

/// Hand a command to the engine and wait for its verdict.
///
/// The timeout exists because a wedged player can leave a D-Bus call outstanding
/// indefinitely, and a client blocking forever on a keybind is worse than one
/// that reports the player is not answering.
async fn run_command(ctx: &Arc<Ctx>, command: Command) -> Frame {
    // Window commands never reach the engine. The engine has no window and no way
    // to know whether one is currently visible; the popup process owns that.
    if let Some(action) = as_popup_action(&command) {
        return route_popup(ctx, anchor(action).await);
    }

    let (reply, answer) = tokio::sync::oneshot::channel();

    if ctx.engine.send(EngineMsg::Command { command, reply: Some(reply) }).await.is_err() {
        return Frame::Error { message: "the engine is not running".into() };
    }

    match tokio::time::timeout(COMMAND_TIMEOUT, answer).await {
        Ok(Ok(Ok(()))) => Frame::Ack,
        Ok(Ok(Err(message))) => Frame::Error { message },
        Ok(Err(_)) => Frame::Error { message: "the engine stopped before answering".into() },
        Err(_) => Frame::Error { message: "the player did not respond in time".into() },
    }
}

/// Fill in a missing anchor with the pointer position.
///
/// Resolved here rather than in the client so that every way of opening the
/// window behaves the same, whether it came from a bar click, a keybind, or
/// another script. A compositor that cannot answer leaves it unset, and the
/// window falls back to a fixed corner.
async fn anchor(action: PopupAction) -> PopupAction {
    match action {
        PopupAction::Show { at: None } => {
            PopupAction::Show { at: waytify_core::compositor::cursor_position().await }
        }
        PopupAction::Toggle { at: None } => {
            PopupAction::Toggle { at: waytify_core::compositor::cursor_position().await }
        }
        other => other,
    }
}

fn as_popup_action(command: &Command) -> Option<PopupAction> {
    match *command {
        Command::TogglePopup { at } => Some(PopupAction::Toggle { at }),
        Command::ShowPopup { at } => Some(PopupAction::Show { at }),
        Command::HidePopup => Some(PopupAction::Hide),
        _ => None,
    }
}

/// Deliver a window action, starting the window if it is not running.
///
/// The popup is spawned lazily and then stays resident, hidden when not in use.
/// Starting GTK on every click costs over a tenth of a second before anything is
/// drawn, which is enough to feel broken, and keeping it resident from boot costs
/// memory to someone who never opens it. Spawning on the first request and hiding
/// afterwards avoids both.
fn route_popup(ctx: &Arc<Ctx>, action: PopupAction) -> Frame {
    // Deliberately the Full subscriber count rather than whether the broadcast
    // has receivers. Every client subscribes to the channel including the bar,
    // which ignores window actions, so receiver count answers "is anything
    // connected" when the question is "is the window connected".
    if ctx.watchers.full.load(Ordering::Relaxed) > 0 {
        let _ = ctx.popup.send(action);
        return Frame::Ack;
    }

    // No window running. Hiding one that does not exist is already true rather
    // than an error.
    if matches!(action, PopupAction::Hide) {
        return Frame::Ack;
    }

    let at = match action {
        PopupAction::Show { at } | PopupAction::Toggle { at } => at,
        PopupAction::Hide => None,
    };
    match spawn_popup(at) {
        Ok(()) => Frame::Ack,
        Err(e) => Frame::Error { message: format!("could not start the player window: {e:#}") },
    }
}

/// Start `waytify popup`, detached, showing immediately.
///
/// Same process group and reaping treatment as the daemon itself gets from the
/// bar: a new group so a signal aimed at the daemon does not take the window with
/// it, and a thread to wait on the child so it does not become a zombie held for
/// the daemon's lifetime.
fn spawn_popup(at: Option<Point>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command as Proc, Stdio};

    let exe = std::env::current_exe().context("locating the waytify binary")?;
    let mut cmd = Proc::new(exe);
    cmd.arg("popup").arg("--show");
    if let Some(p) = at {
        cmd.arg("--at").arg(format!("{},{}", p.x, p.y));
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("spawning the popup process")?;

    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

async fn set_scope(ctx: &Arc<Ctx>, current: &mut Option<Scope>, requested: Scope) {
    if let Some(previous) = *current {
        if previous == requested {
            return;
        }
        ctx.watchers.counter(previous).fetch_sub(1, Ordering::Relaxed);
    }
    ctx.watchers.counter(requested).fetch_add(1, Ordering::Relaxed);
    *current = Some(requested);
    notify_attention(ctx).await;
}

/// Record whether this client's window is on screen.
async fn set_watching(ctx: &Arc<Ctx>, current: &mut bool, active: bool) {
    if *current == active {
        return;
    }
    *current = active;
    if active {
        ctx.watchers.watching.fetch_add(1, Ordering::Relaxed);
    } else {
        ctx.watchers.watching.fetch_sub(1, Ordering::Relaxed);
    }
    notify_attention(ctx).await;
}

async fn notify_attention(ctx: &Arc<Ctx>) {
    let _ = ctx.engine.send(EngineMsg::Attention(ctx.watchers.attention())).await;
}

async fn send_state(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    scope: Scope,
    state: &Arc<State>,
    ctx: &Arc<Ctx>,
) -> Result<()> {
    let frame = match scope {
        // Rendered here so that format strings stay in one config file and the
        // bar binary carries no formatting logic.
        Scope::Bar => Frame::Bar { bar: render_bar(state, &ctx.config.bar) },
        Scope::Full => Frame::State { state: Box::new(State::clone(state)) },
    };
    send(writer, &frame).await
}

async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, frame: &Frame) -> Result<()> {
    writer.write_all(frame.to_line()?.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_follows_the_most_demanding_client() {
        let w = Watchers::default();
        assert_eq!(w.attention(), Attention::Idle);

        w.bar.fetch_add(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Bar);

        // Connecting is not watching. The window stays subscribed while hidden,
        // and polling the Spotify API and lrclib on its behalf the whole time it
        // sits in the background is exactly the waste this distinguishes.
        w.full.fetch_add(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Bar, "a hidden window is not watching");

        // An open popup draws a scrubber, so it outranks any number of bars.
        w.watching.fetch_add(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Popup);

        w.watching.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Bar, "hiding it goes back to the bar's pace");

        w.full.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Bar);

        w.bar.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Idle, "no clients means no polling");

        // A window on its own, with no bar running at all, still counts.
        w.full.fetch_add(1, Ordering::Relaxed);
        w.watching.fetch_add(1, Ordering::Relaxed);
        assert_eq!(w.attention(), Attention::Popup);
    }
}
