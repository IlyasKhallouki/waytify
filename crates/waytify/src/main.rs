//! One binary, several roles. `waytify daemon` holds the state, `waytify bar`
//! feeds Waybar, and the rest are one-shot commands meant for clicks and keybinds.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use waytify_core::config::Config;
use waytify_core::format::{SeekSpec, parse_seek};
use waytify_ipc::{Command as Cmd, Repeat, paths};

#[derive(Parser)]
#[command(
    name = "waytify",
    version,
    about = "Media control for Waybar",
    long_about = "Media control for Waybar, built on MPRIS with optional Spotify enrichment.\n\
                  Run `waytify bar` from a Waybar custom module; the daemon starts on demand."
)]
struct Cli {
    /// Config file. Defaults to $XDG_CONFIG_HOME/waytify/config.toml
    #[arg(long, global = true, env = "WAYTIFY_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Role,
}

#[derive(Subcommand)]
enum Role {
    /// Run the state engine. Started automatically by any client.
    Daemon,
    /// Stream Waybar JSON on stdout. Use this as a custom module's `exec`.
    Bar,

    /// Run the player window. Normally started on demand rather than by hand.
    Popup {
        /// Show as soon as it starts, rather than waiting to be told.
        #[arg(long)]
        show: bool,
        /// Where the click landed, as `X,Y` in compositor logical pixels.
        #[arg(long, value_name = "X,Y")]
        at: Option<String>,
    },

    /// Show the player window if hidden, hide it if shown.
    Toggle {
        /// Where to anchor it. Defaults to the current cursor position.
        #[arg(long, value_name = "X,Y")]
        at: Option<String>,
    },

    /// Toggle playback.
    PlayPause,
    Play,
    Pause,
    /// Skip to the next track.
    Next,
    /// Go to the previous track.
    Previous,

    /// Seek. Accepts `1:30` to jump, or `+10` and `-10` to move.
    Seek {
        #[arg(value_name = "POSITION")]
        position: String,
    },

    /// Toggle shuffle.
    Shuffle,
    /// Cycle repeat between off, playlist, and track.
    Repeat,
    /// Set repeat explicitly.
    SetRepeat {
        #[arg(value_enum)]
        mode: RepeatArg,
    },

    /// Bring the player's own window to the front.
    Raise,

    /// Print the current state as JSON.
    Status,
    /// Stop the running daemon.
    Stop,

    /// Print a fully commented default config file.
    Config,

    /// Connect a Spotify account, for likes and Connect devices.
    ///
    /// Opens a browser once. Needs a client id in the config first; the README
    /// explains registering one, which takes a couple of minutes.
    Login,
    /// Forget the stored Spotify credentials.
    Logout,
    /// Save or unsave the current track.
    Like,

    /// Run a fake MPRIS player, for testing without a real one.
    ///
    /// Produces no audio. Useful for trying the window, for reproducing a bug
    /// without involving your music, and for checking that waytify sees players
    /// at all on a machine where it seems not to.
    MockPlayer,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RepeatArg {
    Off,
    Track,
    Playlist,
}

impl From<RepeatArg> for Repeat {
    fn from(r: RepeatArg) -> Self {
        match r {
            RepeatArg::Off => Repeat::Off,
            RepeatArg::Track => Repeat::Track,
            RepeatArg::Playlist => Repeat::Playlist,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Long-running roles log at info; one-shot commands stay quiet unless
    // something is wrong.
    init_tracing(matches!(cli.command, Role::Daemon | Role::MockPlayer));

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(dispatch(cli))
}

async fn dispatch(cli: Cli) -> Result<()> {
    let command = match cli.command {
        Role::Daemon => {
            let path = cli.config.unwrap_or_else(paths::config_file);
            let config = Config::load(&path).context("reading the config file")?;
            return waytify_daemon::run(config).await;
        }
        Role::Bar => return waytify_bar::run_bar().await,

        #[cfg(feature = "popup")]
        Role::Popup { show, at } => {
            waytify_popup::check_environment()?;
            let options = waytify_popup::Options { show_on_start: show, at: parse_point(&at)? };
            // GTK owns the thread it is initialised on and runs its own main
            // loop, so it must not be started from inside the tokio runtime.
            return tokio::task::block_in_place(|| waytify_popup::run(options));
        }
        #[cfg(not(feature = "popup"))]
        Role::Popup { .. } => {
            anyhow::bail!("this build was compiled without the popup feature")
        }

        Role::Toggle { at } => Cmd::TogglePopup { at: parse_point(&at)? },

        Role::Status => {
            let state = waytify_bar::snapshot().await?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            return Ok(());
        }
        Role::Config => {
            print!("{}", default_config_toml()?);
            return Ok(());
        }
        Role::MockPlayer => return waytify_core::mock::run_standalone().await,

        Role::Login => {
            let path = cli.config.clone().unwrap_or_else(paths::config_file);
            let config = Config::load(&path).context("reading the config file")?;
            let client_id = config.spotify.client_id.trim();
            anyhow::ensure!(
                !client_id.is_empty(),
                "no Spotify client id configured.\n\n\
                 Register an application at https://developer.spotify.com/dashboard, \
                 add http://127.0.0.1:{}/callback as a redirect URI, then put the \
                 client id in {}:\n\n[spotify]\nclient_id = \"...\"",
                config.spotify.redirect_port,
                path.display()
            );

            let tokens =
                waytify_core::spotify::auth::login(client_id, config.spotify.redirect_port).await?;
            waytify_core::spotify::auth::save_refresh_token(&tokens.refresh)?;

            // A running daemon holds the token it started with, so tell it to
            // read the new one. Best effort: there may not be a daemon yet, and
            // the next one to start will read the token anyway.
            match waytify_bar::send(waytify_ipc::Command::ReloadSpotify).await {
                Ok(()) => println!("Spotify connected."),
                Err(_) => {
                    println!("Spotify connected. It will be used the next time waytify starts.")
                }
            }
            return Ok(());
        }
        Role::Logout => {
            waytify_core::spotify::auth::forget_refresh_token()?;
            println!("Spotify credentials forgotten.");
            return Ok(());
        }
        Role::Like => Cmd::ToggleLike,

        Role::PlayPause => Cmd::PlayPause,
        Role::Play => Cmd::Play,
        Role::Pause => Cmd::Pause,
        Role::Next => Cmd::Next,
        Role::Previous => Cmd::Previous,
        Role::Shuffle => Cmd::ToggleShuffle,
        Role::Repeat => Cmd::CycleRepeat,
        Role::SetRepeat { mode } => Cmd::SetRepeat { mode: mode.into() },
        Role::Raise => Cmd::RaisePlayer,
        Role::Stop => Cmd::Shutdown,

        Role::Seek { position } => match parse_seek(&position) {
            Some(SeekSpec::Absolute(ms)) => Cmd::Seek { position_ms: ms },
            Some(SeekSpec::Relative(ms)) => Cmd::SeekBy { delta_ms: ms },
            None => anyhow::bail!(
                "could not read {position:?} as a position. \
                 Try `1:30` to jump, or `+10` and `-10` to move."
            ),
        },
    };

    waytify_bar::send(command).await
}

/// Parse an `X,Y` argument into a point.
///
/// Rejected rather than ignored on malformed input: a keybind passing a bad
/// coordinate should say so, not silently open the window in the wrong place.
fn parse_point(arg: &Option<String>) -> Result<Option<waytify_ipc::Point>> {
    let Some(raw) = arg else { return Ok(None) };
    let (x, y) =
        raw.split_once(',').with_context(|| format!("expected a position as X,Y, got {raw:?}"))?;
    let parse = |v: &str, axis| -> Result<i32> {
        v.trim().parse().with_context(|| format!("{axis} of {raw:?} is not a whole number"))
    };
    Ok(Some(waytify_ipc::Point { x: parse(x, "the X")?, y: parse(y, "the Y")? }))
}

/// Log to stderr. The bar writes its payload to stdout, so anything else on that
/// stream would reach Waybar as a malformed module update.
fn init_tracing(long_running: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let default = if long_running { "info" } else { "warn" };
    let filter = EnvFilter::try_from_env("WAYTIFY_LOG").unwrap_or_else(|_| EnvFilter::new(default));

    fmt().with_env_filter(filter).with_writer(std::io::stderr).with_target(false).init();
}

/// A default config with the reasoning inline, so `waytify config > config.toml`
/// produces a file worth editing rather than a wall of bare keys.
fn default_config_toml() -> Result<String> {
    let defaults = toml::to_string_pretty(&Config::default())?;
    Ok(format!(
        "# waytify configuration\n\
         # Every key below is already the default, so an empty file behaves the same.\n\
         # Written to {}\n\
         #\n\
         # Template syntax:\n\
         #   {{name}}   a value: icon, status, title, artist, album, player,\n\
         #             position, duration, percent\n\
         #   [ ... ]   an optional group, dropped when a value inside it is empty.\n\
         #             This is what keeps a separator from dangling when a track\n\
         #             has no artist or no album.\n\
         #\n\
         # Markup in a template is passed through, so <b>{{title}}</b> works.\n\
         # Values are escaped, so an ampersand in a track title is safe.\n\
         \n{defaults}",
        paths::config_file().display()
    ))
}
