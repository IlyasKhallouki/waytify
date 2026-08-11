# Contributing

Bug reports, especially ones with a player other than Spotify, are the most
useful thing right now. MPRIS is a loose specification and every player reads it
slightly differently.

## Getting set up

```sh
git clone https://github.com/IlyasKhallouki/waytify
cd waytify
cargo test --workspace
```

Building the player window needs GTK4 and gtk4-layer-shell:

```sh
# Arch
sudo pacman -S gtk4 gtk4-layer-shell

# Fedora
sudo dnf install gtk4-devel gtk4-layer-shell-devel

# Debian and Ubuntu
sudo apt install libgtk-4-dev libgtk4-layer-shell-dev
```

`cargo build --no-default-features` skips the window entirely and needs none of
them, which is enough for working on the daemon or the bar client.

## Testing without your music

```sh
waytify mock-player &
waytify popup --show
```

`mock-player` serves a fake MPRIS player on the session bus. It produces no
audio, so you can run it while listening to something else, and it is what the
integration tests drive. Give it cover art with
`WAYTIFY_MOCK_ART=/path/to/image.png`.

To watch what the daemon is doing:

```sh
WAYTIFY_LOG=debug waytify daemon
```

`WAYTIFY_SOCKET` points every part at a different socket, so you can run a test
daemon beside a real one without them fighting.

## Before opening a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets   # CI runs this with -D warnings
cargo test --workspace
```

The end-to-end tests need a session bus. They skip rather than fail without one,
so if you see them pass suspiciously fast, check that they actually ran.

## What the code expects of you

**Do not trust players.** Every MPRIS accessor tries the shapes seen in the wild
and treats a surprise as a missing field rather than a failure. A player with one
odd key should cost a missing album name, not a missing track. Capability flags
like `CanSeek` are ignored for the same reason.

**Prefer MPRIS.** Where both MPRIS and the Spotify Web API can answer, MPRIS
wins. It has no rate limit, no Premium gate, no network dependency and no auth.
The Spotify layer must be able to fail completely and leave a working player.

**Nothing runs when nobody is watching.** With no clients connected the daemon
does no D-Bus traffic and no polling at all. Any new periodic work should hold to
that.

**Widget names are public API.** Anything a stylesheet can select is documented
in [`docs/THEMING.md`](docs/THEMING.md). Renaming one breaks themes, so it counts
as a breaking change.

**Say why, not what.** Comments explaining a mechanism the code already states
are noise. Comments explaining why the obvious approach was not taken are the
ones worth writing, and most of the ones in this codebase exist because something
surprising happened.

## Verifying a change

Tests that pass before your fix are decoration. If you are fixing a bug, check
that the test fails against the unfixed code, then fix it. Several of the tests
here exist in the shape they do because that check changed what they asserted.

If a change affects what the window looks like, look at it. If it affects what
the bar emits, run `waytify bar` and read the output.

## Layout

| Crate | Holds |
| --- | --- |
| `waytify-ipc` | Wire protocol and state model. Depends on nothing but serde. |
| `waytify-core` | Engine: MPRIS, config, templates, position clock, art, mock player. No UI. |
| `waytify-daemon` | Socket server, client scopes, command dispatch. |
| `waytify-bar` | Waybar client and one-shot command client. |
| `waytify-popup` | The GTK4 window. |
| `waytify` | Subcommand dispatch. The binary. |

`waytify-core` has no dependency on GTK, on Waybar, or on a window system, which
is what lets the engine be tested with no display attached. Keep it that way.

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains which source owns which
piece of state and why. `docs/devlog/` records how things got the way they are,
including the wrong turns.
