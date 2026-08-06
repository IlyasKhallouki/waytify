# waytify

Media control for Waybar, built on MPRIS with optional Spotify enrichment.

Written in Rust. Themed with CSS.

> **Status: early.** The bar module and transport controls work today. The GTK4
> popup, volume routing, Spotify Connect, and lyrics are not built yet. See
> [roadmap](#roadmap) for what exists and what does not.

## Why this exists

A Waybar custom module is a subprocess whose stdout Waybar reads. The entire
protocol is this:

```json
{"text": "...", "alt": "...", "tooltip": "...", "class": "...", "percentage": 0}
```

`text` becomes a Pango label. `tooltip` becomes another Pango label. `class`
gets appended to the widget's CSS classes. Clicks and scrolls run shell
commands you configure.

There is no way to express a seek bar, album art, a device list, or scrolling
lyrics. That is why nearly every Spotify module for Waybar is a `playerctl`
call in a loop: authors reach the limit of a string and stop there.

waytify splits along that seam. The bar shows what a string can show. Everything
else lives in a separate window that Waybar knows nothing about, which also
means the player half will work under any bar, or under none.

## Install

Requires Rust 1.85 or newer.

```sh
git clone https://github.com/IlyasKhallouki/waytify
cd waytify
cargo install --path crates/waytify
```

That puts a single `waytify` binary in `~/.cargo/bin`. Make sure it is on your
`PATH`.

## Waybar setup

Add a module:

```jsonc
"custom/waytify": {
  "exec": "waytify bar",
  "return-type": "json",
  "on-click": "waytify play-pause",
  "on-click-right": "waytify next",
  "on-click-middle": "waytify previous",
  "on-scroll-up": "waytify seek +10",
  "on-scroll-down": "waytify seek -10",
  "max-length": 45
}
```

Then put `custom/waytify` in one of your `modules-*` arrays.

There is no `interval`. `waytify bar` is a long-lived process that prints a line
when something changes, so the bar updates on the event rather than on a timer.
The daemon starts on its own the first time any client needs it.

Do not set `"escape": true`. waytify escapes track metadata itself and leaves
your template's markup alone, so `<b>{title}</b>` works while an ampersand in a
song title stays safe. Turning on Waybar's own escaping would double-escape the
first and break the second.

A copy of this module and a starting stylesheet live in
[`contrib/waybar/`](contrib/waybar/).

## Styling

The module emits CSS classes describing the current state, so you can style it
from your existing Waybar stylesheet:

```css
#custom-waytify.playing  { color: @green; }
#custom-waytify.paused   { color: @overlay1; }
#custom-waytify.no-player { opacity: 0; }
```

| Class | When |
| --- | --- |
| `playing`, `paused`, `stopped` | Current playback state |
| `no-player` | Nothing is running |
| `liked` | Current track is saved to your library |
| `remote` | Audio is on a Spotify Connect device, not this machine |
| `no-premium` | An account is connected but cannot use playback controls |

The last three arrive with the Spotify layer and are listed here so a stylesheet
written now keeps working later.

## Configuration

Everything has a default, so the config file is optional. To start from a
commented copy of the defaults:

```sh
mkdir -p ~/.config/waytify
waytify config > ~/.config/waytify/config.toml
```

```toml
[player]
# Which player to favour when several are running. Short name or full bus name.
# Preference only breaks ties between players in the same state: whatever is
# actually playing always wins, so the bar cannot disagree with your speakers.
preferred = ["spotify"]

[bar]
format = "{icon}  {title}[ · {artist}]"
tooltip = "{title}[\n{artist}][\n{album}][\n\n{position} / {duration}]"

[bar.icons]
playing = "▶"
paused = "⏸"
stopped = "⏹"
```

### Template syntax

`{name}` inserts a value. Available names are `icon`, `status`, `title`,
`artist`, `album`, `player`, `position`, `duration`, and `percent`. A name
waytify does not recognise is left in the output untouched, so a typo shows up
on your bar instead of quietly disappearing.

`[ ... ]` marks an optional group. It renders only if every value inside it is
non-empty. This is what keeps `{title}[ · {artist}]` from leaving a stranded
separator on a podcast with no artist tag. Groups nest.

Nerd Font users will probably want different icons:

```toml
[bar.icons]
playing = "󰎆"
paused = "󰏤"
stopped = "󰓛"
```

The defaults are plain Unicode so a fresh install renders on any font.

## Commands

Useful as Hyprland keybinds or Waybar click actions.

```sh
waytify play-pause
waytify next
waytify previous
waytify seek 1:30      # jump to a position
waytify seek +10       # move forward ten seconds
waytify seek -10       # and back
waytify shuffle        # toggle
waytify repeat         # cycle off, playlist, track
waytify raise          # bring the player's window forward

waytify status         # current state as JSON
waytify stop           # stop the daemon
```

Every command reports its outcome. `waytify next` with nothing running exits
non-zero and says so, rather than failing quietly in a log you never read.

For Hyprland:

```conf
bindl = , XF86AudioPlay, exec, waytify play-pause
bindl = , XF86AudioNext, exec, waytify next
bindl = , XF86AudioPrev, exec, waytify previous
```

## How it works

Three processes from one binary.

The **daemon** holds the D-Bus connections, the canonical state, and the caches.
It is the only part that talks to the outside world. Waybar respawns its module
process on every config and style reload, so anything expensive to build has to
outlive that.

The **bar client** (`waytify bar`) streams rendered output to stdout and holds no
state. Format strings live in the daemon's config, so this side has no
formatting logic to get out of sync.

The **popup**, once built, will be a `gtk4-layer-shell` surface: album art, a
real scrubber, output device picker, queue, and lyrics.

Playback position is interpolated locally rather than polled. The daemon records
the position it last saw, advances it against the local clock, and re-anchors on
every property change. The MPRIS spec has a `Seeked` signal for this, but players
are inconsistent about emitting it, so nothing here depends on receiving one. A
paused player is never polled at all, and neither is one nobody is watching.

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) has the full picture, including
which source owns which piece of state and why.

## Roadmap

Each stage is usable on its own.

- [x] **Daemon, bar module, transport.** Works with any MPRIS player, including
      mpv and Firefox. No configuration required.
- [ ] **The popup.** GTK4 layer-shell surface with art, scrubber, transport, and
      local volume through PipeWire. Three-layer CSS with hot reload.
- [ ] **Spotify layer.** OAuth via PKCE, likes, queue, Connect devices, and
      playback transfer. Optional throughout: without it you still have a
      working player.
- [ ] **Lyrics and art-derived theming.** Synced lyrics from lrclib, plus album
      art colours exposed to CSS so a theme can follow the record.

### On Spotify Premium

Every write to Spotify's `/me/player/*` endpoints requires Premium. Playback
transfer and remote volume will not work on a free account, and waytify will
hide those controls rather than offer buttons that fail when clicked. Reads are
unaffected, so likes, the queue, and the device list still work.

None of that touches the MPRIS layer. Transport, seeking, and the bar work the
same whether you have Premium, a free account, or no Spotify at all.

## Development

```sh
cargo test --workspace
```

The suite includes an end-to-end test that serves a mock MPRIS player on the
real session bus and drives the engine against it. The mock reports its track id
as a plain string rather than an object path and never emits `Seeked`, both of
which real players do. Those tests skip rather than fail when no session bus is
available.

## License

MIT. See [LICENSE](LICENSE).
