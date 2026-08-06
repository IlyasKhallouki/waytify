# 0001: Getting to a working bar module

The goal for this first stage was narrow: replace a `playerctl` shell script with
something that updates on events instead of a timer, and get the shape of the
project right before any of the harder pieces land.

## Deciding what the bar is

The first real decision was where the bar client's formatting logic should live.
The obvious version has the bar process read the config, render the template, and
print. That is how most Waybar modules work.

It went to the daemon instead. `waytify bar` receives output that is already
rendered and pipes it to stdout, so the client has no config file, no template
parser, and nothing to fall out of sync with. It also means a second client, the
popup, cannot disagree with the bar about what is playing.

The cost is a slightly fatter wire protocol, since `Frame::Bar` carries a
rendered payload rather than raw state. That seems like the right trade for a
process Waybar restarts every time you touch your stylesheet.

## Templates needed optional groups

The default format is:

```
{icon}  {title}[ · {artist}]
```

Plain substitution breaks the moment a track has no artist, which happens
constantly with podcasts and local files. You get a trailing separator sitting
there looking broken.

The fix is a bracket group that renders only when every value inside it is
non-empty, and that nests. It took about forty lines of parser and removed the
need for `format_no_artist`, `format_no_album`, and the combinatorial explosion
that follows.

The other thing the template layer has to get right is escaping. Markup in the
template passes through so `<b>{title}</b>` works, but substituted values are
escaped. That ordering matters more than it looks: plenty of real songs have an
ampersand in the title, and an unescaped one makes Pango discard the entire
label. The bar goes blank and nothing tells you why.

## Not trusting the players

MPRIS is a loose spec and players read it loosely. Writing the metadata parser
against the letter of the spec would have produced something that breaks on
contact with real software.

`xesam:artist` is documented as an array of strings and arrives as a bare string
from some players. `mpris:trackid` is documented as an object path and arrives as
a plain string from others. Lengths come signed or unsigned. Zero sometimes means
unknown and sometimes means zero.

So every accessor tries the shapes that actually occur and treats a surprise as a
missing field rather than a failed parse. A player with one odd key should cost
you an album name, not the whole track.

The same reasoning applies to `CanSeek` and `CanGoNext`. Several players report
those incorrectly, so nothing is gated on them. Show the control, send the call,
reconcile from whatever comes back.

## The position clock

Polling `Position` often enough for a smooth scrubber means a D-Bus round trip
per frame. The clock records the last observed position with the instant it was
observed and advances locally from there.

MPRIS has a `Seeked` signal meant for exactly this, and players are inconsistent
about emitting it. Spotify's Linux client is widely reported not to. Rather than
build on that, the clock re-anchors on every property change, which arrives
regardless of how the player feels about `Seeked`. If the signal does show up it
gets used, and if it never does nothing notices.

Two rules keep the cost at zero when it should be zero. A paused player cannot
drift, so it is never polled. With no clients connected there is nothing
rendering a position, so there is nothing to correct. An idle machine with the
daemon running produces no D-Bus traffic at all.

## playerctld looked like a second player

Discovery matches `org.mpris.MediaPlayer2.*`, which also matches
`org.mpris.MediaPlayer2.playerctld`.

playerctld is a proxy. It re-exports whichever player was last active under its
own name. Left in the candidate list it shows up as a second player mirroring the
real one, and every state change appears to arrive twice. It is now filtered out
by name.

This was caught by looking at the session bus before writing the discovery code
rather than after, which was lucky.

## A bug the first smoke test found

The first end-to-end run looked fine. The daemon started, the bar rendered, the
socket cleaned itself up on shutdown.

Then `waytify next` with no player running printed nothing and exited zero.

The command had reached the engine, failed, and been logged with
`tracing::warn!`. From the daemon's side that is a perfectly handled error. From
the user's side, a keybind did nothing and gave no reason.

Fixing it meant a reply channel per command and an `Ack` frame in the protocol,
so a one-shot client waits for a verdict and exits with it. `waytify next` now
exits non-zero and says "no player is running".

The `Ack` also matters for latency. Without an explicit success frame the client
has to wait out a timeout to conclude that nothing went wrong, which is a
terrible property for something bound to a media key.

## What the integration test corrected

The unit tests cover parsing and arithmetic. They cannot show that the engine
attaches to a player, reacts to a `PropertiesChanged`, or sends transport calls
to the right destination. So there is now a mock MPRIS player served on the real
session bus, deliberately built to be as awkward as the real thing: string track
ids, no `Seeked`.

It failed on first run, asserting that position resets on a track change. The
engine was reporting the old position.

The mock was wrong, not the engine. A real player rewinds when the track changes
and the mock did not, so the engine read the stale position and correctly
believed the player over its own assumption.

But the failure exposed a wrong belief about the code. There is an optimistic
reset of position to zero when the track id changes, added on the theory that
waiting for the player would leave a stale number on screen. That reset never
reaches a client: the same handler re-reads `Position` immediately afterwards and
publishes only at the end. It is a fallback for when that read fails, not the
normal path. The comments now say so.

That is the kind of thing a test earns its keep for. Nothing was broken, but the
code meant something different from what it was documented to mean.

## Where this leaves things

85 tests, including the end-to-end ones, which skip rather than fail on a machine
with no session bus.

What works: any MPRIS player, event-driven bar output, transport, seeking,
shuffle and repeat, and a daemon that starts on demand and gets out of the way.

Next is the popup, which is the largest single piece and the one that proves the
architecture. Everything so far has been arranged so that the popup is a second
client of an existing daemon rather than a rewrite.
