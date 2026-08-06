# Architecture

## The constraint

A Waybar custom module is a subprocess. Waybar reads its stdout and understands
one thing:

```json
{"text": "...", "alt": "...", "tooltip": "...", "class": "...", "percentage": 0}
```

Two Pango labels and a list of CSS classes. Clicks and scrolls run shell
commands. That is the whole surface area, and no amount of cleverness in the
JSON adds a seek bar or an image to it.

So the bar is a readout, not the application. The application is a separate
window, and because that window is a `gtk4-layer-shell` surface rather than a
Waybar plugin, it works under any bar or none at all.

## Processes

Three roles, one binary, dispatched on the subcommand.

```
  MPRIS players ──┐
  PipeWire      ──┤                        ┌── waytify bar    (stdout to Waybar)
  Spotify API   ──┼──> waytify daemon ─────┤
  lrclib.net    ──┘      unix socket       └── waytify popup  (gtk4-layer-shell)
```

The **daemon** owns every connection, the canonical state, the caches, and later
the OAuth tokens. The **clients** hold nothing but the last frame they received.

That split is not tidiness. Waybar respawns its module process on every config
and style reload. If tokens or caches lived in the module, editing your
stylesheet would trigger a re-authentication. The daemon outlives Waybar
entirely. It also means one D-Bus subscription instead of one per client, one
place that owns the Spotify request budget, and a position clock that survives
the popup opening and closing.

Clients subscribe with a scope. `Bar` receives output the daemon has already
rendered, so album art and lyrics are never serialised to a process that would
discard them. `Full` receives the whole model.

## Crates

| Crate | Holds |
| --- | --- |
| `waytify-ipc` | Wire protocol and the state model. Depends on nothing but serde. |
| `waytify-core` | The engine: MPRIS, config, templates, position clock. No UI, no socket. |
| `waytify-daemon` | Socket server, client scopes, command dispatch. |
| `waytify-bar` | Streaming Waybar client and one-shot command client. |
| `waytify` | Subcommand dispatch. The binary. |

`waytify-core` has no dependency on GTK, on Waybar, or on a window system. That
is what lets the engine be driven by a mock player in a test with no display
attached.

## Which source owns which state

MPRIS is fast, local, and free, and structurally cannot express half of what a
full player UI needs. The Spotify Web API can express all of it but is rate
limited, has no push channel, and gates every write behind Premium. Each piece
of state goes to the cheapest source that can answer for it.

| Capability | Source | Cadence | Why there |
| --- | --- | --- | --- |
| Title, artist, album | MPRIS | event | Arrives in `Metadata` with no network call |
| Album art | MPRIS + HTTP | track change | `mpris:artUrl` is a URL to fetch and cache |
| Play state | MPRIS | event | `PlaybackStatus`, reliable everywhere |
| Position | MPRIS | event, interpolated | See below |
| Seek | MPRIS | on release | No account, no network |
| Transport | MPRIS | on click | Never routed through the Web API |
| Shuffle, repeat | Web API, MPRIS fallback | on click | Spotify reports these unreliably over MPRIS |
| Liked | Web API | track change | MPRIS has no vocabulary for it |
| Queue | Web API | popup open | Spotify exposes no MPRIS `TrackList` |
| Connect devices | Web API | 5s, popup only | No push channel exists, so it must be polled |
| Transfer playback | Web API, Premium | on click | The only mechanism there is |
| Output sink, app volume | PipeWire | event | Bluetooth sinks appear here for free |
| Remote volume | Web API, Premium | on release | No local stream exists to attenuate |
| Lyrics | lrclib | track change | Free, no auth |

**MPRIS wins every tie.** It has no rate limit, no Premium gate, no network
dependency, and no auth. The Spotify layer must be able to fail completely and
leave a working player that is only missing likes, queue, and Connect. That is a
constraint on the design, not error handling added afterwards.

## The position clock

Driving a smooth scrubber by polling `Position` means a D-Bus round trip per
frame. Instead the daemon records the last position it observed along with the
instant it observed it, then advances that locally.

The MPRIS spec has a `Seeked` signal for announcing jumps, but players are
inconsistent about emitting it and Spotify's Linux client is widely reported not
to. Rather than depend on it, the clock re-anchors on every property change,
which arrives regardless. `Seeked` is handled when it turns up and nothing
breaks when it does not.

Correction interval depends on who is watching, and on whether there is anything
to correct:

| Watching | Playing | Interval |
| --- | --- | --- |
| Nobody | either | never |
| Bar only | yes | 30s |
| Popup open | yes | 5s |
| anyone | no | never |

A paused player cannot drift, so it is never polled. With no clients connected,
nothing is rendering a position and nothing needs one. Together those two rules
are what keep an idle machine at zero D-Bus traffic.

Two more details. The clock is frozen while the scrubber is being dragged, so
the thumb does not fight the pointer, and it re-anchors optimistically on
release rather than waiting for the player to confirm. And an observed position
within 250ms of the interpolated one is treated as agreement, since that gap is
ordinary round trip latency rather than a seek.

## Player selection

Several players can be on the bus at once. The ordering tries to match what
someone would point at if asked which player they meant:

1. Anything actually playing beats anything idle. What you can hear is what the
   bar should show.
2. Within that, `player.preferred` order decides.
3. Ties break on bus name, so the choice is stable across restarts instead of
   following hash order.

`org.mpris.MediaPlayer2.playerctld` is excluded from discovery. playerctld is a
proxy that re-exports whichever player was last active under its own name, so
leaving it in produces a phantom second player mirroring the real one, and every
state change appears to arrive twice.

## Trusting players as little as possible

Real players disagree with the spec in small ways, and the parsing layer assumes
they will.

`xesam:artist` is specified as an array of strings and arrives as a bare string
from some players. `mpris:trackid` is specified as an object path and arrives as
a plain string from others, which makes `SetPosition` unusable and forces a
relative-seek fallback. Track lengths arrive signed or unsigned. Zero-length is
sometimes used to mean unknown. Every accessor tries the shapes seen in the wild
and treats a surprise as a missing field rather than a missing track.

The same applies to capability flags. Controls are not gated on `CanSeek` or
`CanGoNext`, because several players report those incorrectly. waytify shows the
control, sends the call, and reconciles from whatever state comes back.

## Wire protocol

Newline-delimited JSON over a Unix socket at `$XDG_RUNTIME_DIR/waytify/sock`.
Every frame is exactly one line, so clients read with `lines()` and need no
length prefix.

Clients send `Command`. The daemon replies with `Frame`: `Hello` on connect,
then `Bar` or `State` depending on scope, and `Ack` or `Error` per command.

Commands are acknowledged rather than fired and forgotten. Without that, a
keybind pressed with no player running exits zero and looks like it worked,
while the actual failure sits in a daemon log nobody reads.

`PROTOCOL_VERSION` is reported in `Hello`. Clients exit loudly on a mismatch
instead of guessing, which matters because a `waytify bar` process can survive
an upgrade underneath it.

## Planned: CSS layering

The popup will stack three style providers so a user stylesheet overrides
defaults without replacing them:

```rust
// Baked into the binary.
add_provider(&display, &base, GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
// Regenerated per track from album art: @define-color art_vibrant #C2703A;
add_provider(&display, &art,  GTK_STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
// ~/.config/waytify/style.css, watched and reloaded. Always wins.
add_provider(&display, &user, GTK_STYLE_PROVIDER_PRIORITY_USER);
```

Themeability is a promise not to rename things, so widget names are treated as
public API and documented alongside the defaults.

## Known hard parts

Recorded ahead of time so they are not surprises later.

**tokio inside GTK.** The popup runs glib's main loop, not tokio's. The socket
reader belongs on its own thread with a channel drained from
`glib::spawn_future_local`. Trying to marry the two loops is an afternoon spent
on nothing.

**Click-outside dismissal.** Layer-shell surfaces do not get focus-out for free.
The workable approach is `keyboard-interactivity: on-demand` plus a transparent
full-screen surface behind the popup to swallow the dismissing click.

**Anchoring the popup.** Waybar does not report where a module sits. On Hyprland
the cursor position can be read over its IPC socket at click time, which also
puts the popup on whichever monitor was clicked. Other compositors fall back to
a configured corner.

**Spotify rate limits.** Requests are counted per application over a rolling
window, and there is no push channel for playback state. The rule that keeps
this from producing 429s is to never poll while the popup is closed, and to
refresh like state and lyrics from MPRIS track-change events rather than a
timer.
