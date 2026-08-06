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

MPRIS has a `Seeked` signal meant for exactly this, and support for it varies.
Rather than build on it, the clock re-anchors on every property change, which
arrives regardless. If `Seeked` shows up it gets used, and if it never does
nothing notices.

This was originally written down with a stronger claim attached: that Spotify's
Linux client does not emit `Seeked`. That is a common thing to read and it turned
out to be wrong. See below.

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

## Checking the assumptions against real Spotify

Everything above was built against the spec and a mock. The last step was to run
it against the actual client and find out which of the assumptions were true.

Two were wrong in ways worth recording.

**Spotify emits `Seeked`.** The claim that it does not is easy to find and was
written into three files here before anyone checked. Version 1.2.92.147 emits it
reliably, once per seek. The design does not change, since the point of
re-anchoring on property changes was never Spotify specifically, but the
justification in the comments was repeating a rumour and now cites a measurement.

**`Position` reports zero on a paused, freshly loaded track.** It advances
correctly once playback starts. Worth knowing before someone concludes the
position tracking is broken.

One assumption held up better than expected. `mpris:trackid` really does arrive
as D-Bus type `s`, a plain string, while `SetPosition` requires type `o`, an
object path. The string happens to contain something object-path shaped, so
converting it works, but nothing guarantees that. The relative-seek fallback
written for this exact case is load-bearing rather than defensive.

`HasTrackList` is `false`, which confirms there is no queue to read over MPRIS and
that it will have to come from the Web API.

There was also one anomaly that did not reproduce. An early run showed the track
title unchanged after `next` with the position jumping from 1:16 to 3:23. A clean
re-run performed two track changes in a row, both landing on the new track at
0:00. The first harness had a `timeout` killing the bar client inside a nested
subshell, which is the most likely explanation, but "probably the test" is not the
same as understanding it. Noted here so it is not forgotten if it comes back.

## Three things running it for real turned up

None of them produced wrong output. All three are the kind of thing you only
notice with the process list open, which is an argument for opening it.

The bar spawns a daemon when none is listening, and never reaped it. Stopping the
daemon left `[waytify] <defunct>` sitting under the bar client, and since the bar
runs for the whole session, every daemon restart would have added another. It now
hands the child to a thread that waits on it. Four daemon restarts in a row now
leave zero zombies, where before each one leaked.

The socket was created `0755`. Its directory is `0700`, so nothing could actually
reach it, but a socket that accepts playback commands should not depend on one
directory's mode staying correct. It is now `0600` as well.

The bar reconnects after the daemon goes away, and every reconnect attempt starts
a daemon if none is listening. With a fixed one second delay that is fine when the
daemon is merely restarting and bad when it cannot start at all, since it forks a
process every couple of seconds forever. The delay now doubles up to thirty
seconds and resets once a connection succeeds.

There was also a moment of alarm at three daemons running at once, which looked
like the single-instance guard failing. They were on three different sockets, two
of them scratch paths from test runs, so the guard was working exactly as
intended. Worth writing down mainly as a reminder that `WAYTIFY_SOCKET` makes the
process list misleading.

## Where this leaves things

85 tests, including the end-to-end ones, which skip rather than fail on a machine
with no session bus.

What works: any MPRIS player, event-driven bar output, transport, seeking,
shuffle and repeat, and a daemon that starts on demand and gets out of the way.

Next is the popup, which is the largest single piece and the one that proves the
architecture. Everything so far has been arranged so that the popup is a second
client of an existing daemon rather than a rewrite.
