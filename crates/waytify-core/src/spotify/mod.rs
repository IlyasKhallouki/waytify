//! The optional Spotify layer.
//!
//! Everything here can fail completely and leave a working player. An expired
//! token, no network, or a free account should cost you likes, the device list
//! and playback transfer, and nothing else. That is a constraint on the design
//! rather than error handling bolted on afterwards, so every call returns a
//! result the caller is expected to shrug at.
//!
//! Two facts shape the whole module.
//!
//! There is no push channel for playback state, so anything the Web API knows
//! has to be polled. The rule that keeps this from producing 429s is to poll
//! only while the window is open, and to refresh everything else from MPRIS
//! track changes, which arrive for free.
//!
//! Every write to `/me/player/*` requires Premium. Reads do not. A free account
//! keeps likes and the device list and loses transfer, so the capability is
//! detected once and the UI hides what it cannot do rather than offering a
//! button that fails.

pub mod api;
pub mod auth;

pub use api::{Client, Device};
pub use auth::{SCOPES, Tokens};
