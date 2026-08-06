# 0003: The bug was never in the engine

The follow-up report was short: still broken, and it feels worse now.

Both halves were accurate.

## The theory that was wrong

[0002](0002-the-pause-icon-that-lied.md) reasoned from the code to a plausible
cause: Spotify announces properties in separate signals, the handler applied only
what a signal carried, and a variant-unwrapping helper in the engine had drifted
from the equivalent one in the metadata parser. Every step of that is true. None
of it was the problem.

What was missing was evidence. The fix went in on reasoning alone, verified
against a reproduction that had never failed in the first place. Passing a test
that was already passing proves nothing, and it felt like progress anyway.

## The theory that made it worse

The same change added a re-read of playback status after every property change.
Written as "ask the player rather than trust the signal", which sounds prudent
and is backwards: a signal announcing `Playing` is the player reporting its own
transition, and asking again a millisecond later can catch it mid-change and read
the state it is leaving. That turned an occasional wrong icon into a race that
could produce one on any transition.

The other regression was worse in practice. Reconnection got exponential backoff,
which is right for a daemon that cannot start and wrong for one that is merely
restarting. The backoff only reset on a clean disconnect, so after a few daemon
restarts the bar would sit blank for up to thirty seconds. Restarting the daemon
is exactly what happens repeatedly while someone is working on it.

Both are now separated properly. A signal that announces the state is believed. A
read only fills a gap where nothing announced anything. Reconnecting after a
disconnect waits 250ms and does not back off; only spawning a daemon backs off,
because only spawning forks a process.

## What it actually was

Turning on debug logging and reading it took about a minute and settled the
question immediately. Every `PlaybackStatus` signal, without exception, was
parsed and applied correctly. The engine was never wrong.

Two other things were.

`waytify` was installed to `~/.cargo/bin`. Waybar runs modules with the PATH of
the session that started it, which comes from the compositor at login and does
not include `~/.cargo/bin`. That directory is added by shell config, which
nothing in a graphical login ever reads. So the binary worked perfectly when
typed in a terminal and did not exist as far as Waybar was concerned.

That alone would be an easy diagnosis if it produced an empty widget. It does
not. **Waybar keeps rendering the last output a module produced after that
module's process exits.** So the widget sat frozen on a track from whenever it
last worked, with a play icon that no longer matched anything. A missing binary
presenting as a stale icon is a very good disguise.

And it was hidden by the testing. Every verification run restarted Waybar from a
shell that had exported `~/.cargo/bin`, so the module launched every time it was
checked and died on every restart that came from anywhere else. The test
environment was the only place the bug could not happen.

## The fix

A symlink into `~/.local/bin`, which is on the session PATH on most
distributions, and `restart-interval: 1` on the module so a client that dies for
any reason is respawned rather than leaving a frozen widget behind. Verified by
killing the client and watching it come back, and by restarting Waybar from a
shell with no cargo paths at all.

The README now covers this before anything else in the install section, since
anyone installing with `cargo install` will hit exactly the same thing and see
exactly the same misleading symptom.

## What to take from it

Three attempts, two of them wrong, and the difference was not cleverness. The
first two reasoned from the code. The third read a log.

The reasoning was not even bad. The mechanism it described was real and the code
it changed is better for the change. It was simply answering a question nobody
had asked, and there was no way to tell from the inside, because a wrong theory
about a plausible mechanism feels exactly like a right one until something
external disagrees.

The tell, in hindsight, was reproducing it. Two attempts failed to trigger the
reported bug and both were treated as "not the right path" rather than as the
information they were. A fix for a bug that has never been observed locally is a
guess wearing a diff.
