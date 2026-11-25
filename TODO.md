# Deferred work

Items agreed but deliberately left for later. Nothing here is scheduled; this
file is so the reasoning does not have to be rediscovered.

## Leveler: look-ahead limiter

The limiter is zero-latency today, so a fast peak is turned down by shaving the
waveform rather than pre-attenuating it. True look-ahead needs a 2-3 ms delay
line and the delay reported through CLAP's latency extension.

**Blocker:** the console prints a plug-in's latency but does not compensate for
it, so a look-ahead insert would make that one strip later than the others -
very visible in the mirror demo (two strips on the same bus would comb-filter).
Do it as a pair:

1. host-side delay compensation per strip (pad every other strip by the chain
   latency), or
2. look-ahead off by default, with the delay only when the person enables it.

## Leveler: its own window

The plug-in has five real parameters but no GUI of its own, so it can only be
adjusted through the console's parameter window. A small CLAP GUI extension
window (vertical faders, same look as the console) would make it usable in any
CLAP host, not just this one.

## Player drift on long videos

`trim -0.400 %` and `drift +240 ms/min` on every strip is rev 1 behaviour: the
player and the cable clock run on two independent clocks. Long videos drift out
of step. The fix is to pace the USB transfers from the mixer clock (slave the
cable clock) rather than trimming at the reader.

## Stream Deck

The plugin itself is in `streamdeck/` (Strip Mute + Mute All, live state); see
`streamdeck/README.md`. Left for later:

- **Transport keys** (play/pause, next, previous) and **Bypass all inserts**,
  **Mirror**, **Streamer mode** toggles - all one more `Command` variant each.
- **Global hotkeys** in the console (`RegisterHotKey` on a message-only-window
  thread) feeding the same command queue, so any keyboard or macro pad works,
  not just the deck.
- **Port setting in the plugin**: the console's port is configurable
  (`<Control port='8765'>`), the plugin assumes 8765.
- **Launch the mixer** from the plugin when it is not running (spawn the exe).
- **Dials / touch strip** if a Stream Deck + ever shows up, and the Neo Infobar
  for a now-playing display.

## Console

- CLAP back end reports latency as 0 (`ClapSlot::latency`); VST3 reports the
  real value. Needed before any CLAP plug-in with look-ahead can be trusted.
- The insert panel's `Open` and the insert click both fall back to the parameter
  window when a plug-in has no editor - worth revisiting if a plug-in ever has
  an editor that fails to open for a transient reason, since the fallback then
  hides the error.
