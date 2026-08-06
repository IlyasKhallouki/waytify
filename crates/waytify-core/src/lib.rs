//! The state engine behind waytify.
//!
//! This crate owns everything that knows how to talk to the outside world:
//! MPRIS over D-Bus, and later PipeWire, the Spotify Web API, and lyrics. It has
//! no dependency on GTK, on Waybar, or on a window system, which is what lets the
//! whole model be driven from recorded fixtures in a test with no display attached.
//!
//! Clients never see this crate. They see `waytify_ipc` types over a socket.

pub mod clock;
pub mod config;
pub mod format;
pub mod metadata;
pub mod mpris;

pub use clock::{Attention, PositionClock};
pub use config::Config;
pub use format::render_bar;
pub use metadata::{Metadata, spotify_track_id, track_from_metadata};
