# Changelog

Audio Mixer PRO reports its build in the top bar as `<CHANNEL> · rev <REV>`.
`REV` counts every published build of that release line.

## 1.1 — unreleased

Interface cleanup, plus a demo feature for showing several plug-ins at once.

- **Removed controls that did nothing:** the `A` and `R` top-bar buttons (they
  only showed hardcoded toasts), the strip `K` (karaoke) and `M.C.` buttons, the
  bus `MODE` selector, the bus `SEL` button, the "Monitor on BUS SEL" option,
  and the draggable limiter marker on every meter.
- **The bottom bar is PLAYER, not TAPE.** The cassette transport, file picker,
  bus route chips, `PLAY` gain and the non-functional `REC` button are gone. The
  bar now shows the current track, elapsed/total, previous/play/next, a button
  for the playlist window, and which strip carries the player.
- Removed the "Tape Recorder Options" window and the "Eject Cassette" menu item.
  The tape engine and built-in demo material stay for `--startup-demo`. A file
  dropped from Explorer now only goes to the player playlist; it used to be
  loaded into the hidden tape deck as well.
- **Built-in COMP/GATE removed.** The strip's per-sample gate and compressor (fixed
  threshold, one 0..10 knob, no UI since the rebuild) are gone; dynamics live in
  the strip's VST/CLAP insert, which has real controls and metering. The engine
  no longer writes `<StripComp>`/`<StripGate>`, `karaoke`, `prg_c`, `prg_g`,
  `dblimit` or `muc`; scenes that contain them still load (the parser ignores
  them).
- **Player mirror (demo mode):** "Mirror to every strip" feeds the same decoded
  audio into every other strip's ring, so several plug-in windows can be shown
  on one source with no cable and no offset between strips. Each strip keeps its
  own VST, fader and bus routing; switching the mirror off restores the previous
  input of every strip.
- **Scenes no longer carry dead console state.** The writer stopped emitting
  `paneltype`, `ColorPanel*`, `ModPanel*`, `audibility_c/g`, `muc`, `prg_c/g`,
  the bus `BusMode`/`SEL`/`monitor`, `<VMOptions MonitorOnSEL>`, `engine`,
  `<RecordArm>` and `<RecordParam>`, and the reader ignores them in old files.
  Pan still round-trips in `Panel3Dx`; `RecordMode Loop` is kept for the demo
  tape. The "Engine mode: Normal/Swift" switch was removed from System
  Settings: nothing read it.

## 1.0.1 — rev 2 (2026-11-23)

- **Fix: a plug-in's editor window could jump in front of whatever you clicked.**
  The editor host re-sent keyboard focus to the plug-in's child window whenever
  the mixer's thread had no focus — which is exactly the state after clicking
  another application or the desktop — and `SetFocus` activates that window.
  Focus is now only nudged while the editor itself is the active window, so
  plug-in meters still start but the window stays where it was put.

## 1.0 — rev 1 (2026-11-23)

First stable release. The audio engine is the same code as Beta 6 rev 67;
1.0 changes the identity, the licence and the packaging, not the sound.

- **MIT licence** (`LICENSE`) and `THIRD-PARTY-NOTICES.txt`.
- **Playlist highlight**: the playing track is drawn with a green row and a play
  marker; the loaded-but-paused track is amber with a pause marker.
- Version reported as `1.0 · rev 1`; `Cargo.toml` and the Windows file
  version move to 1.0.0.
- Documentation refresh: this changelog, an updated `README.txt`, a
  `QUICK START` section and the known issues below.
- Installer (Inno Setup) alongside the portable zip.
- Known issues carried into 1.0: occasional clicks with several heavy inserts,
  and the virtual cable driver remains experimental and test-signed.

## Beta 6 — rev 67

- `PLAYER` button in the top bar, right of `DUCKING`. It lights while the player
  window is open and creates the player (and its strip) on first use.
- The transient status message moved right and is narrower so it cannot cover
  the new button.

## Beta 6 — rev 66

- **Fix: Add Files... is genuinely multi-select.** The dialog was given
  `OFN_NONETWORKBUTTON`'s value (`0x20000`) where `OFN_ALLOWMULTISELECT`
  (`0x200`) belongs, so Ctrl and Shift could not extend the selection and only
  one file was returned. The result buffer decoder is now a separate function
  with tests for the one-file and folder-plus-names layouts.

## Beta 6 — rev 65

- **Add Files... selects several files at once.**
- **Drag & drop from Explorer**: audio files, folders (three levels deep,
  sorted) and `.xspf`/`.m3u`/`.m3u8`/`.pls` playlists append to the player's
  list, and the player window opens.
- **Move up / Move down** in the track context menu.
- The player can be moved to another strip from its Strip picker; the previous
  strip returns to `No input`.

## Beta 6 — rev 64

- **Internal streaming player** (`MENU > Player...`). Decodes on a worker thread
  into the same ring an input capture uses, so the player feeds one strip and
  gets that strip's fader, pan, EQ, compressor/gate, VST slot, ASIO insert and
  bus sends. No Windows audio engine and no virtual cable in the path.
- **No 30-minute limit**: packets are streamed instead of decoding the whole
  file into RAM.
- **Playlists**: XSPF (VLC), M3U/M3U8 and PLS, with VLC's `<vlc:item tid>`
  playback order, UTF-8 percent decoding (emoji filenames) and durations from
  the playlist. Saved in the scene as `<Player>`/`<PlayerTrack>`, reloaded
  paused.
- Transport: play/pause/stop, previous/next, scrub, double-click to play,
  right-click to remove, add a file, save as XSPF, repeat OFF/ONE/ALL, shuffle.
- **Crossfade** 0–10 s (default 3 s), equal-power; 0 gives a gapless join.
- 750 ms decode-ahead absorbs disk and CPU hiccups.
- FLAC and OGG/Vorbis decoding.

Why it exists: while VLC played into a virtual cable, VLC's own Windows audio
session dropped to 0.000 for 0.4–1.9 s every two to five minutes, and the mixer
faithfully passed the silence on. The same material played through the ASIO
insert from Waveform had no dips, which located the fault in the Windows audio
path rather than in the console.

## Beta 6 — rev 63

- `Insert 1` / `Insert 2` buttons under every strip header, with the active pair
  green and `PRE`/`POST`.
- `CLK` switch on the A1 bus card: the A1 output device's clock steers the
  internal mixer timer, with a bounded trim and a 100 ms fallback to the nominal
  timer.
- The external ASIO insert assignment is saved in the scene.

## Beta 6 — rev 58

- Build identity in the top bar (`Beta 6 · rev 58`) with the commit in the
  tooltip.
- Direct cable taps are the default: a strip reads the cable driver's own
  buffer instead of opening a Windows capture stream, removing one buffer, one
  clock and the USB round trip from the input path.
- Endpoint format shown on every strip and bus ("32-bit · 48 kHz").
- Diagnostics report: one text file with build, devices, cables, plug-ins,
  latency and the last few hundred log lines, from the CABLES panel or
  `--diagnostics <path>`.
- Output streams keep their ring cushion again (rate servo with an integrator).
- Bus limiter transparent below about −0.9 dBFS.
- Empty device lists no longer close every output stream.
- Plug-in windows no longer freeze the console; vsync off, 30 fps pacing, UI
  watchdog.
- Plug-in crash guard: the plug-in loading when the mixer died is not restored.
- Reattach removes stale USB device nodes before attaching.
- Smaller executable (10.4 MB) after repairing an encoding fault.

## Beta 5 — rev 42

- Fixes: clicks with a loaded insert (lock collision played dry blocks),
  distortion with gain-adding inserts (tanh limiter), silence on a starved bus
  ring, UI freezes from plug-in message pumps, window hang from vsync.
- Fixed-width latency readout with a per-bus breakdown.
- Plug-in crash guard, reattach repair, insert panel click behaviour, meters
  moved to the real post-insert level.

## Earlier betas

Release notes for Beta 1–4 are in the `distribution` folder with their
packages.
