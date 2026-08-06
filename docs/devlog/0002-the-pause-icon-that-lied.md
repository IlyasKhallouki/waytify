# 0002: The pause icon that lied

The report: pause, then pick a different song. Spotify starts playing it. The bar
keeps showing the pause icon. Title and elapsed time update correctly, so only
the playback state is wrong.

## Failing to reproduce it twice

The obvious repro is `playerctl next` while paused. Spotify stays paused after
that, so nothing to see. Second attempt was `OpenUri`, which is the closest
programmatic equivalent of clicking a track. Playback started and the bar
followed it correctly.

Then some time went into driving the actual Spotify GUI, which mostly proved that
clicking around someone else's music client with synthetic input is a poor use of
an afternoon. Stopped, and went back to what the signal captures already showed.

## What the captures showed

Spotify announces each property in its own `PropertiesChanged`. Selecting a track
while paused produces something like:

```
PropertiesChanged { PlaybackStatus: "Playing" }
PropertiesChanged { Metadata: {...} }
```

Two independent signals. The handler treated each one as complete: whatever
properties a signal carried got applied, and anything absent was left alone. So
the state was only ever as correct as the last signal that happened to mention
it.

Three separate weaknesses fed into that.

`as_str` in the engine matched `Value::Str` but not `Value::Value`, the nested
variant form. The equivalent helper in the metadata parser had handled both from
the start, and the engine's copy had drifted. Anything arriving wrapped was
silently dropped rather than misread, which is the worst kind of failure to
notice, because everything else keeps working.

`PlaybackStatus` was declared as a normal cached property. zbus maintains that
cache from its own `PropertiesChanged` handler, so reading it while processing
the same signal that updates it is a race.

And the recovery path could not run. The reconciliation timer only ticked while
playback was believed to be playing, on the reasoning that a paused player's
position cannot drift. That reasoning is correct about position and wrong about
status. If the state gets stuck at paused, the one mechanism that would have
noticed is exactly the one that is switched off.

That last part is what turns a dropped signal into a permanent wrong icon rather
than a brief one.

## The fix

Stop treating a signal as the source of truth. A signal now means "something
changed, go ask", and the engine re-reads playback status and position from the
player on every property change. `PlaybackStatus` is marked uncached so that read
is a real call rather than zbus's cache.

The paused case now gets a thirty second reconciliation tick. It costs one D-Bus
call every thirty seconds while paused with something watching, and it means a
missed signal heals on its own instead of persisting until the track changes. The
idle case is untouched: with no clients connected, nothing is polled at all.

`as_str` and `as_bool` unwrap nested variants, matching what the metadata parser
already did.

## Making the test earn its place

The regression test emits a track change that starts playback and deliberately
withholds the status signal. The only way to pass is to ask the player.

It was then checked against the old behaviour by disabling the status re-read,
where it fails with `left: Paused, right: Playing`. A regression test that passes
before the fix is decoration, and there was no way to know which this one was
without trying it.

## Worth remembering

The bug was not in the parsing, the clock, or the protocol. It was an assumption:
that a change notification tells you what changed. It tells you something
changed. Those are different, and the gap between them is exactly one dropped
signal wide.
