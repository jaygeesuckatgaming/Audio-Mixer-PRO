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

## A/V sync: settled 2026-11-25

The "sound ahead of the lips" report was **Twitch in Chromium**, not the console.
Evidence gathered on this machine:

- with the rate servos disabled, the strip ring drained 80 -> 10 ms in under a
  minute (~1200 ppm) and starved, so a fill controller is required;
- with the controller replaced by a **proportional** loop both loops hold: strip
  fill flat at the target, `trim +-46 ppm`, `held` constant, `starve 0`,
  underruns 0, and the measured tap rate is 48000 +/-25 ppm - i.e. the cable
  clock and A1 agree to well under 50 ppm, which is 15 ms per 5 minutes;
- VLC playing a local file **through the mixer**: in sync;
- YouTube in Chrome **through the mixer**: in sync after 6+ minutes;
- Twitch in Chrome **and** Edge: drifts, audible offset at ~4-5 minutes.

So the drift belongs to Twitch's player (Chromium media path), and nothing in the
audio engine needs to change for it. Practical workaround for watching Twitch:
`streamlink twitch.tv/<channel> best` (hands the stream to VLC).

Two controller bugs were fixed on the way and are worth remembering:

1. an **integrator** controlling a fill that is itself an integral of the trim is
   a double integrator: it limit-cycled at +-0.4 % (240 ms/min) and the delay
   moved by tens of ms. Both loops are now proportional-only.
2. the tap inputs were drained once per mixer block, so a late block delivered
   several device periods at once and the strip ring sawtoothed by ~40 ms. Taps
   now have their own 2 ms thread.

Still open from this thread: the console's A1 cushion is 42 ms for physical
devices (USB cables get 60); if a future machine starves, that is the knob.
Cable-clock slaving (pacing the emulated USB transfers from A1) was **not**
needed: the two clocks agree to within the measurement noise.

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
