#![windows_subsystem = "windows"]

mod audio;
mod clap_host;
mod clap_plugin;
mod clap_slot;
mod editor_host;
mod host_window;
mod plugin;
mod sessions;
mod vban;
mod vc;
mod vst;

use audio::{AudioEngine, DemoKind, InputCapture, BASE_STRIPS, MAX_STRIPS};
use raw_window_handle::HasWindowHandle;
use eframe::egui::{
    self, Align2, Color32, FontId, Id, Pos2, Rect, Response, Sense, Stroke, Vec2,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const BG: Color32 = Color32::from_rgb(13, 15, 19);
const PANEL: Color32 = Color32::from_rgb(26, 30, 37);
const PANEL_HI: Color32 = Color32::from_rgb(34, 39, 48);
const TRACK: Color32 = Color32::from_rgb(8, 9, 12);
const LINE: Color32 = Color32::from_rgb(47, 54, 65);
const TEXT: Color32 = Color32::from_rgb(216, 222, 231);
const DIM: Color32 = Color32::from_rgb(124, 134, 150);
const FAINT: Color32 = Color32::from_rgb(83, 92, 106);
const AMBER: Color32 = Color32::from_rgb(224, 158, 55);
const BLUE: Color32 = Color32::from_rgb(76, 158, 214);
const TEAL: Color32 = Color32::from_rgb(70, 176, 154);
const RED: Color32 = Color32::from_rgb(196, 68, 58);
const YELLOW: Color32 = Color32::from_rgb(214, 178, 62);
const GREEN: Color32 = Color32::from_rgb(86, 192, 124);
const PURPLE: Color32 = Color32::from_rgb(146, 118, 208);

const GAIN_MIN: f32 = -60.0;
const GAIN_MAX: f32 = 12.0;
const GAIN_STEP: f32 = 3.0;

/// Inline application list in a virtual strip: header plus three rows, so the
/// block keeps a stable height whether or not apps are playing.
const APPS_HEADER_H: f32 = 11.0;
const APPS_ROW_H: f32 = 22.0;
const APPS_VISIBLE_ROWS: usize = 3;
const APPS_BLOCK_H: f32 = APPS_HEADER_H + APPS_ROW_H * APPS_VISIBLE_ROWS as f32;

// Menu command ids from FUN_0052faa0, handled by FUN_00543640.
const CMD_RESTART_ENGINE: u32 = 80; // 0x50
const CMD_AUTO_RESTART_A1: u32 = 85; // 0x55
const CMD_AUTO_RESTART_ALL: u32 = 69; // 0x45

// Constants recovered from the original (addresses are in MECHANICS.md).
// Fader drag: FUN_00541ce0 moves 72 dB across the control height minus a
// fixed pad (64 px on a strip, 56 px on a bus), see 0x56de70.
const FADER_TRAVEL_DB: f32 = 72.0; // 0x56de70 (f64)
const STRIP_FADER_PAD: f32 = 64.0; // FUN_00541ce0, controls 0xd4-0xd8
const BUS_FADER_PAD: f32 = 56.0; // FUN_00541ce0, controls 0xd9-0xdd
// Knobs (pan / EQ / compressor / gate) step 0.1 per pixel, 0x56dba0.
const KNOB_STEP_PER_PX: f32 = 0.1;
// L/R PAN control range, FUN_00530520 -> 0x56dbe8 / 0x56dbe4.
const PAN_MIN: f32 = -0.5;
const PAN_MAX: f32 = 0.5;
// FUN_0052b490 reads three audibility fields and clamps each to 0..10
// (0x56dce8): +0x30 "audibility", +0x34 "audibility_c" (the COMP knob) and
// +0x38 "audibility_g" (the GATE knob). FUN_00530520 gives controls
// 0x4ba-0x4bc and 0x4c4-0x4c6 that same 0..10 range.
const AUDIBILITY_MIN: f32 = 0.0;
const AUDIBILITY_MAX: f32 = 10.0; // 0x56dce8
// FUN_0052b490 clamps the three virtual-input EQ bands to -12..+12 dB
// (0x56de68 and 0x56db6c).
const EQ_GAIN_MIN: f32 = -12.0; // 0x56de68
const EQ_GAIN_MAX: f32 = 12.0; // 0x56db6c
// FUN_00541ce0 controls 0x114-0x116 (the hardware 3D panel this rebuild
// draws): the 143 x 128 control keeps a 14 px inset on each side and a 15 px
// bottom / 25 px top inset (height - 40). X maps to -0.5..+0.5 and Y to 0..1:
//   x = (px - rect.x - 14) / (rect.w - 28) - 0.5
//   y = (rect.bottom - py - 15) / (rect.h - 40)
// FUN_00541510 resets the active pair to (0,0), i.e. bottom centre, not the
// middle. The separate virtual-strip pads 0x123/0x124 use an 8 px inset and
// map BOTH axes to -0.5..+0.5 over 59 x 52 px.
const PANEL_X_MIN: f32 = -0.5; // 0x56dbe8
const PANEL_X_MAX: f32 = 0.5; // 0x56dbe4
const PANEL_Y_MIN: f32 = -0.5; // virtual pads 0x123/0x124
const PANEL_Y_MAX: f32 = 0.5; // virtual pads 0x123/0x124
const PANEL3D_Y_MIN: f32 = 0.0; // hardware panel 0x114-0x116
const PANEL3D_Y_MAX: f32 = 1.0;
const PANEL_W: f32 = 143.0; // panel rect 0x57cd54 (x=7, y=129)
const PANEL_H: f32 = 128.0;
const PANEL_X_INSET: f32 = 14.0; // (rect.w - 28) leaves 115 px of travel
const PANEL_Y_TOP_INSET: f32 = 25.0; // (rect.h - 40) leaves 88 px
const PANEL_Y_BOTTOM_INSET: f32 = 15.0;
// The virtual-strip pads (controls 0x123/0x124) use an 8 px inset over a
// 59 x 52 px control; this rebuild has no virtual pad, so that branch is
// recorded in MECHANICS.md rather than drawn.
// Brickwall limiter: -40 dB minimum (0x56dd30) and +12 dB maximum
// (0x56db6c). FUN_00541ce0's dedicated limiter controls 0x96-0x98 use 60 dB
// of travel over their 60 px control (0x56df58 / 0x57c7e8). The six meter
// cells 200-205 (0xc8-0xcd) write the same +0x20 dblimit field but use
// 78.0 dB over the cell's full height (0x56de10, height 234 at 0x57c770).
const LIMIT_MIN: f32 = -40.0;
// Strip meter scale (FUN_00537110, the six 234 px columns built from
// 0x57c764..0x57c7d4 at x 13/27, 173/187, 333/347): the lit height is
//   pixels = (dB + 66.0) * 3.0
// with 0x56de28 = 66.0 and 0x56dcf8 = 3.0. Over the 234 px bitmap that is
// -66 dB at the bottom and +12 dB at the top, a 78 dB span, and the 0 dB
// point sits at (0 + 66) * 3 = 198 px = 84.6% of the way up - the same place
// as 0 dB on the -60..+12 fader travel.
const METER_MIN_DB: f32 = -66.0;
const METER_MAX_DB: f32 = 12.0;
const METER_SPAN_DB: f32 = 78.0; // METER_MAX_DB - METER_MIN_DB
// The bus meters use their own spans in the original: the eight 60 px
// columns at 0x57c7dc use (dB + 68.0) * 0.75 (0x56de20 / 0x56de18), and the
// five master-section banks of eight 136 px columns at 0x57c818 use
// (dB + 78.0) * 1.522222 (0x56de10 / 0x56de08). A single rebuilt bus bar
// follows the master-section window: -78..+12 dB.
const BUS_METER_MIN_DB: f32 = -78.0;
const BUS_METER_MAX_DB: f32 = 12.0;
const BUS_METER_SPAN_DB: f32 = BUS_METER_MAX_DB - BUS_METER_MIN_DB; // 90 dB
// The original's lit meter segments are the skin's light blue, but the
// person asked to keep this rebuild's earlier dark-console palette, so the
// existing green/amber/red zones stay. Only the dB window they are drawn in
// is corrected here (they were mapped over 66 dB but fed a 78 dB scale).
const METER_UNLIT: Color32 = Color32::from_rgb(22, 26, 30);
// FUN_00537110 converts a sample to dB with a log2 table: 20*log10(2) =
// 6.0206 dB per exponent step (0x56ddd0), a -200 dB value for silence
// (threshold 1e-10 at 0x56dde0, result 0x56ddd8) and a -100 dB floor
// (0x56de30) when the per-strip maximum is accumulated.
const METER_DB_PER_OCTAVE: f32 = 6.020_599_8;
const METER_SILENCE_DB: f32 = -200.0;
const METER_FLOOR_DB: f32 = -100.0;
const METER_SILENCE_LINEAR: f32 = 1.0e-10;
// FUN_00537110 prints "-" for the numeric readout at or below this value
// (0x56de00 = -96.0 f32); otherwise it prints 20*log10 as a whole number.
const METER_READOUT_DB: f32 = -96.0;
// FUN_00538130 reloads a 10-tick hold once the 50-tick hold expires; the
// console timer runs every 15 ms, so the bar follows in 150 ms steps.
// The rebuild no longer copies that step-and-snap motion: the person asked
// for meters that glide like a normal VU meter. The hold value is still used
// for the peak marker.
const METER_HOLD_S: f32 = 0.75;
// VU-style ballistics. The original has no smoothing constant to copy (it
// jumps to each new peak), so these two time constants are a rebuild choice,
// recorded as estimated in mechanics.json. The bar is filtered in dB, rising
// quickly and falling more slowly, which is the motion of a moving-coil meter.
const VU_ATTACK_S: f32 = 0.06;
const VU_RELEASE_S: f32 = 0.18;
// Even when the source jumps from silence to full level, the bar may not cross
// more than this much of the scale in one frame. 240 dB/s gives the roughly
// 300 ms full-scale rise of a VU movement; the fall is a little slower.
const VU_MAX_RISE_DB_S: f32 = 240.0;
const VU_MAX_FALL_DB_S: f32 = 200.0;
const METER_PEAK_FALL_DB_S: f32 = 18.0;

// FUN_00538fa0 writes the strip dynamics block and shows where the DSP
// fields live inside the 0x7c-byte strip record:
//   compressor +0x3c gainin, +0x40 attack, +0x44 release, +0x48 knee,
//              +0x4c comprate, +0x50 threshold, +0x58 automakeup,
//              +0x5c gainout
//   gate       +0x60 thresin, +0x64 damping, +0x68 bpsidechain,
//              +0x6c attack, +0x70 hold, +0x74 release
// FUN_0052b490 reads every other strip field but never these blocks, and the
// file has no "StripComp"/"StripGate" element-name string outside the writer,
// so in 2.1.2.2 the dynamics values are engine-fixed. The compressor preset
// copied by FUN_00541510 comes from 0x584abc (nine dwords):
const COMP_GAININ_DB: f32 = 0.0; // 0x584abc
const COMP_ATTACK_MS: f32 = 10.0; // 0x584ac0
const COMP_RELEASE_MS: f32 = 50.0; // 0x584ac4
const COMP_KNEE: f32 = 0.5; // 0x584ac8
const COMP_RATIO: f32 = 1.0; // 0x584acc
const COMP_THRESHOLD_DB: f32 = -40.0; // 0x584ad0
const COMP_AUTOMAKEUP: u32 = 1; // 0x584ad8
const COMP_GAINOUT_DB: f32 = 0.0; // 0x584adc
// Gate fields as written by FUN_00538fa0. The installed scene uses these
// values; the 0..10 knob is audibility_g, not the gate threshold.
const GATE_THRESHOLD_DB: f32 = -60.0;
const GATE_DAMPING_DB: f32 = -60.0;
const GATE_SIDECHAIN_PCT: f32 = 100.0;
const GATE_ATTACK_MS: f32 = 20.0;
const GATE_HOLD_MS: f32 = 200.0;
const GATE_RELEASE_MS: f32 = 1250.0;

// FUN_0052cd90 (the RecordParam reader) accepts only these values. It shifts
// BitRes right by 3 and keeps 1..4, caps preRecTime at DAT_0057c6e8 (= 20),
// and treats any nbChannel other than 4/6/8 as 2.
const REC_FILE_TYPES: [i32; 4] = [1, 2, 3, 100];
const REC_BIT_DEPTHS: [u32; 4] = [8, 16, 24, 32];
const REC_TARGET_RATES: [u32; 9] = [
    22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
];
const REC_CHANNELS: [u32; 4] = [2, 4, 6, 8];
const REC_KBPS: [u32; 14] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const REC_PRE_REC_MAX_S: i32 = 20; // DAT_0057c6e8
// Default recorder values used by the console.
const REC_FILE_TYPE: i32 = 3;
const REC_BIT_DEPTH: u32 = 16;
const REC_KBPS_DEFAULT: u32 = 192;
const REC_PRE_REC_DEFAULT: i32 = 0;

// FUN_00527ad0 accepts exactly these main-stream rates (32000, 0xac44,
// 48000, 0x15888, 96000, 0x2b110, 0x2ee00) and rejects a main buffer above
// 0x800 samples. 0x581dd8 / 0x582328 name 7168 samples as the VAIO default.
const SAMPLE_RATES: [u32; 7] = [32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
const MAIN_BUFFER_MAX: u32 = 2048;
const VAIO_BUFFER_DEFAULT: u32 = 7168;

// FUN_0052ab70 reads <OptionDev> and accepts MME/WDM/KS buffers in
// 128..=2048 samples (0x80..0x900); ASIO is either 0 or 64..=1024
// (0x40..0x400). The three static defaults are at 0x57c6ec = 1024 (MME),
// 0x57c6f0 = 512 (WDM) and 0x57c6f4 = 512 (KS).
const DEV_BUFFER_MIN: u32 = 128;
const DEV_BUFFER_MAX: u32 = 2048;
const ASIO_BUFFER_MAX: u32 = 1024;
const DEV_MME_DEFAULT: u32 = 1024;
const DEV_WDM_DEFAULT: u32 = 512;
const DEV_KS_DEFAULT: u32 = 512;
// FUN_0052ab70 checks msA1/msA2/msA3 against 0.0 .. 0.5 * 1000.0
// (f32 0x57c708 = 0.5, f64 0x56dcc8 = 1000.0), so the accepted range is
// 0..=500 ms; the globals at 0x6edff0/4/8 start at 0.
const DEV_MS_MAX: f32 = 500.0;
// Device type codes in <InputDev type='%i'> / <OutputDev type='%i'>:
// FUN_004cd550 compares the dialog's type selector with 1, 4, 8 and 0x100.
const DEVICE_TYPE_MME: i32 = 1;
const DEVICE_TYPE_WDM: i32 = 4;
const DEVICE_TYPE_KS: i32 = 8;
const DEVICE_TYPE_ASIO: i32 = 0x100;
// FUN_0052ab70 stores each OutputDev pOut1..pOut8 entry after clamping it to
// 0..=0x40 (64), the eight output-channel patch numbers.
const OUT_PATCH_MAX: i32 = 64;

// FUN_0053e7b0 cases 0x12f (PLAY) and 0x130 (REC) move the playhead back to
// the start of its current second before starting:
//     position_ms = (position_ms / 1000) * 1000
// The console stores the tape position in milliseconds (scene +0x23c4/0x23c7),
// so the rebuild's position in seconds is floored to the same 1 s grid.
const TAPE_POS_GRID_S: f32 = 1.0;

// BusMode is stored as bits 4..7 of the bus flag word. The numeric values
// come from FUN_00528b40 / FUN_00539f00, so the array is in stored order
// (2 = Repeat, 3 = Mixdown B), not in the order the mode button cycles.
const MODE_NAMES: [&str; 12] = [
    "NORMAL",
    "MIX A",
    "REPEAT",
    "MIX B",
    "COMPOSITE",
    "TV MIX",
    "UP 2.1",
    "UP 4.1",
    "UP 6.1",
    "CENTER",
    "LFE",
    "REAR",
];

const MODE_HELP: [&str; 12] = [
    "Normal: multichannel signal passes unchanged",
    "Mixdown A: 5.1/7.1 folded to stereo (rear-left minus side-left)",
    "Stereo repeat: channels 1+2 repeated to 3+4, 5+6, 7+8",
    "Mixdown B: 5.1/7.1 folded to stereo (rear-left plus side-left)",
    "Composite: BUS channels filled from selected inputs",
    "TV Mix: 7.1 created from a stereo source",
    "Upmix 2.1: stereo plus LFE (50% L+R)",
    "Upmix 4.1: stereo plus LFE and rear channels",
    "Upmix 6.1: stereo plus LFE, rear and side channels",
    "Center only: L = R = centre channel",
    "LFE only: L = R = sub-woofer channel",
    "Rear only: L = rear-left, R = rear-right",
];

// FUN_0053e7b0 (bus mode button, controls 0x119-0x11d) advances the stored
// BusMode value in this exact order: normal, A mix, B mix, repeat, composite,
// TV mix, then the up-mixes and the three single-channel modes.
fn next_bus_mode(mode: usize) -> usize {
    match mode {
        0 => 1,
        1 => 3,
        3 => 2,
        2 => 4,
        4 => 5,
        5 => 6,
        6 => 7,
        7 => 8,
        8 => 9,
        9 => 10,
        10 => 11,
        11 => 0,
        _ => 1,
    }
}

// FUN_00539f00 treats BusMode 12..15 through the switch default, i.e. as
// Normal. Keep the raw stored number but display the Normal slot.
fn mode_slot(mode: usize) -> usize {
    if mode < MODE_NAMES.len() {
        mode
    } else {
        0
    }
}

// The original's meters are fed by the engine's per-channel peak values
// (FUN_00538130) and drawn in FUN_00537110; there is no synthetic motion in
// the console. This rebuild feeds them from the capture and output peaks, so
// the constants for a fake wobble are gone.

fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn lin_to_db(lin: f32) -> f32 {
    // FUN_00537110: silence below 1e-10 is -200 dB; otherwise the level is
    // log2(amplitude) * 6.0206, the same exponent/table method as the
    // original. The visible meter is floored at METER_MIN_DB.
    if lin <= METER_SILENCE_LINEAR {
        METER_SILENCE_DB
    } else {
        lin.log2() * METER_DB_PER_OCTAVE
    }
}

/// Smooth VU-style meter motion. FUN_00538130 (0x538244-0x53827c) jumps the
/// bar straight to a new peak, holds it for 50 timer ticks and then snaps to
/// the live value; that is the stepping the person asked to fix. The rebuild
/// keeps the original's dB scale and 0.75 s peak hold, but moves the bar
/// through a one-pole filter in dB with separate rise and fall constants, so
/// it glides continuously instead of jumping between values.
fn follow_meter(level: &mut f32, peak: &mut f32, hold: &mut f32, target: f32, dt: f32) {
    let target_db = lin_to_db(target).clamp(METER_FLOOR_DB, METER_MAX_DB + 6.0);
    let level_db = lin_to_db(*level).clamp(METER_FLOOR_DB, METER_MAX_DB + 6.0);
    let tau = if target_db >= level_db {
        VU_ATTACK_S
    } else {
        VU_RELEASE_S
    };
    let step = 1.0 - (-dt.max(0.0) / tau).exp();
    let desired = (target_db - level_db) * step;
    let max_step = if desired >= 0.0 {
        VU_MAX_RISE_DB_S
    } else {
        VU_MAX_FALL_DB_S
    } * dt.max(0.0);
    let next_db = level_db + desired.clamp(-max_step, max_step);
    *level = if next_db <= METER_FLOOR_DB + 0.05 {
        0.0
    } else {
        db_to_lin(next_db)
    };

    // Peak marker: follows a rising bar, then holds for the original 0.75 s
    // and slides down in dB at 18 dB/s, so the marker does not snap either.
    if *level > *peak {
        *peak = *level;
        *hold = METER_HOLD_S;
    } else {
        *hold = (*hold - dt.max(0.0)).max(0.0);
        if *hold <= 0.0 {
            let peak_db = lin_to_db(*peak).clamp(METER_FLOOR_DB, METER_MAX_DB + 6.0);
            let level_db = lin_to_db(*level).clamp(METER_FLOOR_DB, METER_MAX_DB + 6.0);
            let next = (peak_db - dt.max(0.0) * METER_PEAK_FALL_DB_S).max(level_db);
            *peak = if next <= METER_FLOOR_DB + 0.05 {
                0.0
            } else {
                db_to_lin(next)
            };
        }
    }
}

/// Console check for the rebuilt meter motion. It drives `follow_meter` with a
/// bursty, music-like peak sequence and with a silence-to-full step, then
/// reports the largest change between two 60 fps frames. A working meter never
/// exceeds the configured slew limits (about 8 dB up, 5 dB down per frame).
fn meter_selftest() {
    let dt = 1.0f32 / 60.0;
    let mut level = 0.0f32;
    let mut peak = 0.0f32;
    let mut hold = 0.0f32;
    let mut prev_db = lin_to_db(level).max(METER_FLOOR_DB);
    let mut max_up = 0.0f32;
    let mut max_down = 0.0f32;
    // A bursty signal: loud for 120 ms, quiet for 40 ms, with small jitter.
    for i in 0..900 {
        let t = i as f32 * dt;
        let burst = (t * 5.0).fract() < 0.75;
        let jitter = ((t * 37.0).sin() * 0.5 + 0.5) * 0.15;
        let target = if burst { 0.35 + jitter } else { 0.01 };
        follow_meter(&mut level, &mut peak, &mut hold, target, dt);
        let db = lin_to_db(level).max(METER_FLOOR_DB);
        if i > 0 {
            let d = db - prev_db;
            if d >= 0.0 {
                max_up = max_up.max(d);
            } else {
                max_down = max_down.max(-d);
            }
        }
        prev_db = db;
    }
    // Full-scale step out of silence, the worst case for a visible leap.
    let mut level = 0.0f32;
    let mut peak = 0.0f32;
    let mut hold = 0.0f32;
    let mut prev_db = lin_to_db(level).max(METER_FLOOR_DB);
    let mut step_max = 0.0f32;
    for i in 0..120 {
        follow_meter(&mut level, &mut peak, &mut hold, 1.0, dt);
        let db = lin_to_db(level).max(METER_FLOOR_DB);
        if i > 0 {
            step_max = step_max.max((db - prev_db).abs());
        }
        prev_db = db;
    }
    println!(
        "meter selftest: steady max_up={max_up:.2} dB/frame max_down={max_down:.2} dB/frame; silence-to-0dB max_step={step_max:.2} dB/frame; limits up={:.2} down={:.2}",
        VU_MAX_RISE_DB_S * dt,
        VU_MAX_FALL_DB_S * dt
    );
}

fn gain_y(rect: Rect, db: f32) -> f32 {
    let t = ((db - GAIN_MIN) / (GAIN_MAX - GAIN_MIN)).clamp(0.0, 1.0);
    rect.bottom() - t * rect.height()
}

// FUN_00541ce0 drags a strip fader over (control height - 64 px) and a bus
// fader over (control height - 56 px). The knob is drawn over the same
// inset travel, so pointer movement and the picture agree.
fn fader_travel(rect: Rect, pad: f32) -> Rect {
    let inset = (pad * 0.5).clamp(0.0, rect.height() * 0.45);
    Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + inset),
        Pos2::new(rect.right(), rect.bottom() - inset),
    )
}

fn fmt_db(db: f32) -> String {
    if db <= GAIN_MIN + 0.05 {
        "-inf".to_string()
    } else {
        format!("{:+.1} dB", db)
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum StripKind {
    Hardware,
    Virtual,
}

#[derive(Clone)]
struct Strip {
    kind: StripKind,
    name: String,
    device: String,
    gain_db: f32,
    pan: f32,
    comp: f32,
    gate: f32,
    /// FUN_0052b490 / FUN_00528b40 strip flags 0x10 (`prg_c`) and 0x20
    /// (`prg_g`): the engine's "compressor / gate program active" bits.
    /// FUN_00528410 clears them whenever the matching knob is adjusted and
    /// FUN_00541510 clears them on double click, so they only survive a
    /// scene load untouched.
    comp_prg: bool,
    gate_prg: bool,
    limit: f32,
    eq: [f32; 3],
    intelli: [f32; 2],
    /// FUN_0052b490 "paneltype": 0 = Pan (0x100000 clear, 0x200000 clear),
    /// 1 = Color (flag 0x100000), 2 = Mod/Fx (flag 0x200000).
    panel_type: u8,
    /// +0x10 / +0x14 ColorPanelx / ColorPanely (x -0.5..+0.5, y 0..1).
    color_panel: [f32; 2],
    /// +0x18 / +0x1c ModPanelx / ModPanely (x -0.5..+0.5, y 0..1).
    mod_panel: [f32; 2],
    route: [bool; audio::MAX_BUSES],
    mute: bool,
    solo: bool,
    mono: bool,
    mc: bool,
    karaoke: u8,
    /// Virtual Input 1 meters the tape deck while it keeps its original
    /// "Virtual 1 (VAIO)" device; choosing a Windows endpoint or "No input"
    /// turns this off.
    tape_feed: bool,
    meter: f32,
    peak: f32,
    /// Peak-hold time in seconds (FUN_00538130 holds a new maximum for
    /// 50 timer ticks at 15 ms, then follows the live value).
    hold: f32,
}

impl Strip {
    fn new(i: usize) -> Self {
        let hw = i < 3;
        let mut route = [false; audio::MAX_BUSES];
        if i < BASE_STRIPS {
            route[0] = true;
        }
        Strip {
            kind: if hw { StripKind::Hardware } else { StripKind::Virtual },
            name: if hw {
                format!("HW INPUT {}", i + 1)
            } else {
                format!("VIRTUAL {}", i - 2)
            },
            device: if hw {
                match i {
                    0 => "WDM: Microphone",
                    1 => "WDM: Line In",
                    _ => "WDM: Aux In",
                }
                .to_string()
            } else if i == 3 {
                "Virtual 1 (VAIO)".to_string()
            } else if i == 4 {
                "Virtual 2 (AUX)".to_string()
            } else {
                "No input".to_string()
            },
            gain_db: 0.0,
            pan: 0.0,
            comp: 0.0,
            gate: 0.0,
            comp_prg: false,
            gate_prg: false,
            limit: 12.0,
            eq: [0.0, 0.0, 0.0],
            intelli: [0.0, 0.0],
            panel_type: 0,
            color_panel: [0.0, 0.0],
            mod_panel: [0.0, 0.0],
            route,
            mute: false,
            solo: false,
            mono: false,
            mc: false,
            karaoke: 0,
            tape_feed: i == 3,
            meter: 0.0,
            peak: 0.0,
            hold: 0.0,
        }
    }
}

/// A VBAN receive binding feeding one strip. The hub owns the socket and
/// writes decoded frames into `shared`; the strip pipeline is identical to a
/// Windows capture.
struct VbanInput {
    shared: Arc<audio::InputShared>,
    device: String,
    status: String,
}

impl VbanInput {
    fn new(shared: Arc<audio::InputShared>, device: String) -> Self {
        Self {
            shared,
            device,
            status: "VBAN listening".to_string(),
        }
    }

    fn sample_rate(&self) -> u32 {
        self.shared.rate_value().max(8_000)
    }
}

enum StripInput {
    Cpal(InputCapture),
    Vban(VbanInput),
}

impl StripInput {
    fn device(&self) -> &str {
        match self {
            Self::Cpal(capture) => &capture.device,
            Self::Vban(input) => &input.device,
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            Self::Cpal(capture) => capture.sample_rate,
            Self::Vban(input) => input.sample_rate(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            Self::Cpal(capture) => capture.channels,
            Self::Vban(_) => 2,
        }
    }

    fn available(&self) -> bool {
        match self {
            Self::Cpal(capture) => capture.available,
            Self::Vban(input) => !input.status.is_empty(),
        }
    }

    fn status(&self) -> &str {
        match self {
            Self::Cpal(capture) => &capture.status,
            Self::Vban(input) => &input.status,
        }
    }

    fn peak(&self) -> f32 {
        match self {
            Self::Cpal(capture) => capture.peak(),
            Self::Vban(input) => input.shared.peak_value(),
        }
    }

    fn ring_fill_ms(&self) -> f32 {
        match self {
            Self::Cpal(capture) => capture.ring_fill_ms(),
            Self::Vban(input) => input.shared.fill_ms(input.sample_rate()),
        }
    }

    fn last_frames(&self) -> usize {
        match self {
            Self::Cpal(capture) => capture.last_frames(),
            Self::Vban(input) => input.shared.frames_last(),
        }
    }

    fn shared(&self) -> Arc<audio::InputShared> {
        match self {
            Self::Cpal(capture) => capture.shared(),
            Self::Vban(input) => input.shared.clone(),
        }
    }
}

#[derive(Clone, Copy)]
struct StripClip {
    gain_db: f32,
    pan: f32,
    comp: f32,
    gate: f32,
    comp_prg: bool,
    gate_prg: bool,
    limit: f32,
    eq: [f32; 3],
    intelli: [f32; 2],
    panel_type: u8,
    color_panel: [f32; 2],
    mod_panel: [f32; 2],
    route: [bool; audio::MAX_BUSES],
    mute: bool,
    solo: bool,
    mono: bool,
    mc: bool,
    karaoke: u8,
}

impl StripClip {
    /// "Copy All" in FUN_00538860 (command 0x262): the whole Strip block
    /// written by FUN_00538fa0, minus the label.
    fn capture(s: &Strip) -> Self {
        StripClip {
            gain_db: s.gain_db,
            pan: s.pan,
            comp: s.comp,
            gate: s.gate,
            comp_prg: s.comp_prg,
            gate_prg: s.gate_prg,
            limit: s.limit,
            eq: s.eq,
            intelli: s.intelli,
            panel_type: s.panel_type,
            color_panel: s.color_panel,
            mod_panel: s.mod_panel,
            route: s.route,
            mute: s.mute,
            solo: s.solo,
            mono: s.mono,
            mc: s.mc,
            karaoke: s.karaoke,
        }
    }

    /// "Paste" (command 0x26c) applies the copied block to another strip.
    fn apply(self, s: &mut Strip) {
        s.gain_db = self.gain_db;
        s.pan = self.pan;
        s.comp = self.comp;
        s.gate = self.gate;
        s.comp_prg = self.comp_prg;
        s.gate_prg = self.gate_prg;
        s.limit = self.limit;
        s.eq = self.eq;
        s.intelli = self.intelli;
        s.panel_type = self.panel_type;
        s.color_panel = self.color_panel;
        s.mod_panel = self.mod_panel;
        s.route = self.route;
        s.mute = self.mute;
        s.solo = self.solo;
        s.mono = self.mono;
        s.mc = self.mc;
        s.karaoke = self.karaoke;
    }
}

#[derive(Clone, Copy)]
struct EqBand {
    on: bool,
    freq: f32,
    gain: f32,
    q: f32,
}

#[derive(Clone)]
struct Bus {
    name: String,
    device: String,
    gain_db: f32,
    mute: bool,
    mono: u8,
    eq_on: bool,
    sel: bool,
    monitor: bool,
    mode: usize,
    meter: f32,
    peak: f32,
    hold: f32,
    eq: [EqBand; 6],
}

impl Bus {
    fn new(i: usize) -> Self {
        let names = ["A1", "A2", "A3", "B1", "B2", "B3"];
        let devices = [
            "WDM: Speakers",
            "WDM: Headphones",
            "WDM: Line Out",
            "Virtual Cable 01",
            "Virtual Cable 02",
            // B3 has no automatic cable: pick a device (or a VBAN stream).
            "",
        ];
        let freqs = [50.0, 200.0, 800.0, 2000.0, 8000.0, 12000.0];
        let mut eq = [EqBand {
            on: true,
            freq: 100.0,
            gain: 0.0,
            q: 3.0,
        }; 6];
        for (b, f) in eq.iter_mut().zip(freqs.iter()) {
            b.freq = *f;
        }
        Bus {
            name: names[i].to_string(),
            device: devices[i].to_string(),
            gain_db: 0.0,
            mute: false,
            mono: 0,
            eq_on: false,
            sel: false,
            monitor: false,
            mode: 0,
            meter: 0.0,
            peak: 0.0,
            hold: 0.0,
            eq,
        }
    }
}

struct Tape {
    playing: bool,
    recording: bool,
    pos: f32,
    len: f32,
    file: String,
    /// Built-in demo material (generated in audio.rs), if any.
    demo: Option<DemoKind>,
    /// Full path of a loaded WAV file, if any.
    path: String,
    gain_db: f32,
    route: [bool; audio::MAX_BUSES],
    /// RecordMode Loop: restart at the end instead of stopping.
    loop_playback: bool,
    /// <RecordParam> values, read with FUN_0052cd90's acceptance rules.
    rec_file_type: i32,
    rec_bit_depth: u32,
    rec_target_sr: u32,
    rec_channels: u32,
    rec_kbps: u32,
    rec_pre_rec: i32,
    /// <RecordMode TimerStop>: a non-negative second count, not a flag.
    timer_stop: i32,
    /// <RecordArm> bits: ArmIn1-8 in 0..7, ArmBus1-8 in 16..23.
    arm: u32,
}

impl Tape {
    fn new() -> Self {
        Tape {
            playing: false,
            recording: false,
            pos: 0.0,
            len: 252.0,
            file: "No file loaded - click to choose".to_string(),
            demo: None,
            path: String::new(),
            gain_db: 0.0,
            route: [true, false, false, false, false, false],
            loop_playback: false,
            rec_file_type: REC_FILE_TYPE,
            rec_bit_depth: REC_BIT_DEPTH,
            rec_target_sr: 48_000,
            rec_channels: 2,
            rec_kbps: REC_KBPS_DEFAULT,
            rec_pre_rec: REC_PRE_REC_DEFAULT,
            timer_stop: 0,
            arm: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum Picker {
    Strip(usize),
    Bus(usize),
}

/// One VST3 insert owned by one input strip. Each strip gets its own chain, its
/// own loaded plug-in, its own bypass flag and its own state file, so two
/// channels can run different plug-ins at the same time.
struct VstSlot {
    chain: std::sync::Arc<plugin::PlugIn>,
    path: String,
    bypass: bool,
    info: Option<vst::VstInfo>,
    error: Option<String>,
    /// Plug-ins inside a chain wrapper (UhbikWrapper), refreshed slowly so the
    /// strip can list them under the wrapper's name.
    sub_plugins: Vec<String>,
    sub_plugins_at: f64,
}

impl VstSlot {
    fn new(chain: std::sync::Arc<plugin::PlugIn>) -> Self {
        VstSlot {
            chain,
            path: String::new(),
            bypass: false,
            info: None,
            error: None,
            sub_plugins: Vec::new(),
            sub_plugins_at: 0.0,
        }
    }

    fn loaded(&self) -> bool {
        self.info.is_some()
    }
}

/// Staged start-up. Each step runs on its own frame, after the window has
/// painted, so a blocked audio device only stalls that step instead of keeping
/// the whole console from appearing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupStep {
    Engine,
    Cables,
    Devices,
    Done,
}

/// Ducking settings. One source strip keys the attenuation of any number of
/// target strips; persisted in the scene as `<Ducking>`.
#[derive(Clone)]
struct Ducking {
    enabled: bool,
    source: Option<usize>,
    targets: u32,
    depth_db: f32,
    threshold_db: f32,
    attack_ms: f32,
    release_ms: f32,
}

impl Default for Ducking {
    fn default() -> Self {
        Self {
            enabled: false,
            source: None,
            targets: 0,
            depth_db: -12.0,
            threshold_db: -35.0,
            attack_ms: 10.0,
            release_ms: 300.0,
        }
    }
}

struct MixerApp {
    strips: Vec<Strip>,
    /// Visible strips (5..=MAX_STRIPS). Vectors stay MAX_STRIPS long so a
    /// strip's audio/VST slot index never moves while it is active.
    active_strips: usize,
    /// Which page of up to five strips the console is showing.
    strip_bank: usize,
    /// Diagnostics only: add strips after the virtual cables attach.
    pending_late_virtual_adds: usize,
    /// Extra strips added before their virtual cable was enumerated; retried
    /// once per watchdog tick until a free cable appears or the user picks one.
    pending_auto_assign: Vec<usize>,
    buses: Vec<Bus>,
    /// One real capture stream per hardware strip (None until a device opens).
    captures: Vec<Option<StripInput>>,
    tape: Tape,
    audio: AudioEngine,
    sample_rate: u32,
    buffer: u32,
    main_buffer: u32,
    /// FUN_0052ab70 / FUN_00529c80 `<OptionDev>`: per-driver buffer sizes,
    /// the "use preferred ASIO rate" flag and the three output latencies.
    dev_mme: u32,
    dev_wdm: u32,
    dev_ks: u32,
    dev_asio: u32,
    dev_srasio: bool,
    bus_latency_ms: [f32; 3],
    /// FUN_0052ab70 stores `<InputDev type>` / `<OutputDev type>` at record
    /// +0x284: 1 MME, 4 (default WDM), 8 KS, 0x100 ASIO.
    input_types: [i32; 3],
    output_types: [i32; audio::MAX_BUSES],
    /// `<OutputDev pOut1..pOut8>` for an unassigned output (name "-").
    output_patch: [[i32; 8]; audio::MAX_BUSES],
    engine_mode: usize,
    limit_remote: bool,
    monitor_on_sel: bool,
    lock_gui: bool,
    help_caption: bool,
    menu_open: bool,
    /// Ignore the outside-click test on the frame that opened the menu; the
    /// MENU button click is outside the popup and would close it immediately.
    menu_ignore_click: bool,
    show_system: bool,
    show_vc: bool,
    vc: crate::vc::VirtualCables,
    vc_cfg: virtual_cables::Config,
    show_about: bool,
    show_audio_stats: bool,
    duck: Ducking,
    show_ducking: bool,
    /// Bus rename dialog: which bus is being edited and the draft name.
    bus_rename: Option<usize>,
    bus_rename_text: String,
    bus_rename_focus: bool,
    /// Last inner size in points while the window is not maximized, saved so
    /// the console reopens at the size it was left at.
    window_size: Option<[f32; 2]>,
    /// Last outer position in points, saved together with the size.
    window_pos: Option<[f32; 2]>,
    /// Whether the window was maximized when it was closed.
    window_maximized: bool,
    /// Effective egui zoom factor (Ctrl+scroll / Ctrl +/-), saved with the
    /// window so the UI reopens at the same scale.
    ui_zoom: f32,
    /// Streamer mode: a running light chases across every fader.
    streamer_mode: bool,
    /// Chase speed in faders per second.
    streamer_speed: f32,
    /// Maximize once on the first frame: winit ignores with_maximized() for an
    /// undecorated window, but the runtime command works.
    pending_maximize: bool,
    /// Native handle of the mixer window (0 until the first frame). Plug-in
    /// editor windows are placed relative to it.
    mixer_hwnd: isize,
    /// `--open-vst-editor <slot>`: open one strip's plug-in window shortly after
    /// start-up so the editor path can be reproduced without clicking.
    open_editor_once: Option<usize>,
    /// `--vst-log`: write each loaded plug-in slot's processing counters to
    /// startup.log once a second.
    vst_log: bool,
    vst_log_clock: f64,
    /// `--vst-shot <path>`: save the open editor's picture six seconds in,
    /// from inside the app so nothing steals the editor's focus first.
    vst_shot: Option<String>,
    show_vban: bool,
    vban: vban::VbanHub,
    vban_enabled: bool,
    vban_port: u16,
    vban_quality: u8,
    vban_target_strip: usize,
    vban_out: Vec<vban::VbanOutConfig>,
    show_tape_opts: bool,
    /// FUN_0052d990 / FUN_0052de50 preset scene browser: a saved scene with
    /// PresetName / PresetComment / PresetTimeStamp around the parameter block.
    show_presets: bool,
    preset_name: String,
    preset_comment: String,
    preset_entries: Vec<PresetEntry>,
    preset_status: String,
    preset_active: String,
    show_preset_on_startup: bool,
    show_info: Option<(String, String)>,
    /// Applications rendering into each strip's virtual cable (WASAPI
    /// sessions on the cable's render endpoint), listed inline in the strip.
    apps: sessions::AppSessions,
    apps_rows: HashMap<String, Vec<sessions::AppSession>>,
    app_icons: HashMap<String, egui::TextureHandle>,
    apps_open: [bool; MAX_STRIPS],
    apps_scroll: [usize; MAX_STRIPS],
    apps_targets: Vec<String>,
    /// One VST3 insert per input strip (rebuild extension, see vst.rs).
    vsts: Vec<VstSlot>,
    /// Which strip's VST selector is open, if any.
    vst_panel: Option<usize>,
    vst_scan: Option<std::sync::mpsc::Receiver<vst::ScanOutcome>>,
    vst_scanning: bool,
    vst_scan_results: Vec<vst::VstInfo>,
    vst_scan_errors: Vec<String>,
    vst_status: String,
    vst_filter: String,
    /// Diagnostic: close normally after this many seconds (used to test the
    /// save-on-exit path without a human).
    auto_exit: Option<f64>,
    bus_eq_open: Option<usize>,
    picker: Option<Picker>,
    /// One flag per bus: a feedback loop was already announced, so the toast
    /// fires on the transition instead of every frame.
    loop_warned: [bool; audio::MAX_BUSES],
    label_edit: Option<usize>,
    label_buf: String,
    /// In-app clipboard used by the strip context menu's Copy All / Paste
    /// (FUN_00538860 commands 0x262 and 0x26c).
    strip_clip: Option<StripClip>,
    tape_picker: bool,
    tape_error: Option<String>,
    tape_info: Option<String>,
    tape_recent: Vec<PathBuf>,
    tape_path_input: String,
    status: String,
    status_until: f64,
    time: f64,
    frame_no: u64,
    shot_path: Option<PathBuf>,
    shot_requested: bool,
    first_frame: bool,
    startup_demo: Option<usize>,
    /// Startup is staged across frames so the window is painted before any
    /// audio call that can block inside the Windows audio service.
    startup_step: StartupStep,
    startup_wait: f64,
    startup_settle: f64,
    /// `--safe-mode` / `AMP_SAFE=1`: no engine, no captures, no cables, no
    /// VSTs, no session scanning. Enough UI to fix a broken scene.
    safe_mode: bool,
    /// Menu 0x55 "Auto Restart Audio Engine (A1 Device)": after 5 one-second
    /// ticks with A1 unavailable or the device list changed, re-open silently
    /// (FUN_0054b730, FUN_00527ad0(1, 1)).
    auto_restart_a1: bool,
    /// Menu 0x45 "Auto Restart Audio Engine (All Device)".
    auto_restart_all: bool,
    /// "Load Settings on Startup" (the original's default): FUN_0052f0b0
    /// loads the saved scene file (or the registry startup path) before the
    /// first paint.
    load_on_startup: bool,
    /// The original saves the scene twice in WM_DESTROY
    /// (FUN_00529c80 at 0x54beb1 and 0x54bec2), so edits survive a restart
    /// even when the person never pressed Save Settings.
    save_on_exit: bool,
    poll_clock: f64,
    /// In-flight `output_devices()` probe. Enumeration blocks when the audio
    /// service is wedged, so it never runs on the UI thread.
    devices_probe: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    /// Same for capture devices: the auto-restart watchdog must never call
    /// `InputCapture::devices()` on the UI thread.
    inputs_probe: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    /// Last time each probe was started, so a picker that asks every frame
    /// cannot spawn a thread and enumerate devices at frame rate.
    devices_probe_at: f64,
    inputs_probe_at: f64,
    last_inputs: Vec<String>,
    /// Set once the saved captures have been applied from a probed list, so a
    /// start-up that raced the probe still ends up with its inputs open.
    inputs_applied: bool,
    /// Per strip backoff for reopening a vanished capture device, so a dead
    /// endpoint cannot be hammered once a second.
    capture_retry_at: [f64; audio::MAX_STRIPS],
    capture_backoff: [f64; audio::MAX_STRIPS],
    /// Capture opens running on a worker. `InputCapture::open` touches the audio
    /// service and blocks when an endpoint is wedged, so the UI never calls it.
    capture_jobs: Vec<(usize, std::sync::mpsc::Receiver<audio::InputCapture>)>,
    /// `--meter-log`: write each strip's capture peak to startup.log once a
    /// second. Used to tell "no signal in the capture" apart from "routing".
    meter_log: bool,
    meter_clock: f64,
    /// How often the engine has been reopened to reach the configured A1
    /// device. A cable endpoint can show up a moment after the mix starts.
    a1_reopen_tries: u8,
    restart_ticks: u32,
    last_outputs: Vec<String>,
    /// True when the kernel virtual-cable driver's endpoints are in use
    /// instead of the USB/IP cables.
    driver_mode: bool,
    /// Whether the virtual cables were attached on the previous watchdog tick.
    /// The first attach after startup means any capture opened earlier can be
    /// stale, so the two virtual strips are re-opened once.
    vc_was_attached: bool,
}

// ---------------------------------------------------------------------------
// Settings XML (FUN_00528b40 / FUN_00529c80 / FUN_0052b490 / FUN_0052c030)
// ---------------------------------------------------------------------------
//
// The original writes one `AudioMixerProSettings` document. The scene
// block is `<AudioMixerProParameters>` and every element is a self-contained
// tag; the loader matches tags by name, not by position, so this rebuild can
// write the elements it models and leave the rest out.

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Attribute value in one element's attribute text. Accepts single or double
/// quotes, as the original does (`BusMode='%i'`, `name="%s"`).
fn xml_attr(attrs: &str, name: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = attrs[from..].find(name) {
        let at = from + rel;
        let before_ok = at == 0 || !(bytes[at - 1] as char).is_ascii_alphanumeric() && bytes[at - 1] != b'_';
        let end_name = at + name.len();
        let after_ok = end_name >= bytes.len()
            || !(bytes[end_name] as char).is_ascii_alphanumeric() && bytes[end_name] != b'_';
        if before_ok && after_ok {
            let mut i = end_name;
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'=' {
                i += 1;
                while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && (bytes[i] == b'\'' || bytes[i] == b'"') {
                    let q = bytes[i];
                    let start = i + 1;
                    if let Some(rel_end) = attrs[start..].find(q as char) {
                        return Some(attrs[start..start + rel_end].to_string());
                    }
                }
            }
        }
        from = at + name.len();
    }
    None
}

/// Every `<tag ...>body</tag>` or `<tag ... />` in `text`.
fn xml_elements<'a>(text: &'a str, tag: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let mut from = 0usize;
    loop {
        let Some(rel) = text[from..].find(&format!("<{tag}")) else {
            break;
        };
        let start = from + rel;
        let name_end = start + 1 + tag.len();
        let boundary = text[name_end..]
            .chars()
            .next()
            .map(|c| c == '>' || c == '/' || c.is_ascii_whitespace())
            .unwrap_or(false);
        if !boundary {
            from = name_end;
            continue;
        }
        let Some(gt_rel) = text[start..].find('>') else {
            break;
        };
        let gt = start + gt_rel;
        let attrs = &text[start..gt];
        if text[start..gt].trim_end().ends_with('/') {
            out.push((attrs, ""));
            from = gt + 1;
            continue;
        }
        let close = format!("</{tag}>");
        if let Some(close_rel) = text[gt + 1..].find(&close) {
            let close_at = gt + 1 + close_rel;
            out.push((attrs, &text[gt + 1..close_at]));
            from = close_at + close.len();
        } else {
            from = gt + 1;
        }
    }
    out
}

fn f32_attr(attrs: &str, name: &str) -> Option<f32> {
    xml_attr(attrs, name).and_then(|v| v.trim().parse().ok())
}

fn bool_attr(attrs: &str, name: &str) -> Option<bool> {
    xml_attr(attrs, name).map(|v| v.trim() != "0")
}

// ---------------------------------------------------------------------------
// Preset scenes (FUN_0052d990 writer / FUN_0052de50 reader)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PresetEntry {
    path: PathBuf,
    name: String,
    comment: String,
    stamp: String,
}

fn presets_dir() -> PathBuf {
    settings_file().with_file_name("presets")
}

/// FUN_0052d990 stamps the preset with GetLocalTime in
/// `YYYY/MM/DD - HHhMM:SS` form.
fn preset_stamp_now() -> String {
    #[cfg(windows)]
    unsafe {
        // SAFETY: GetLocalTime only writes the SYSTEMTIME it is handed.
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut st);
        return format!(
            "{:04}/{:02}/{:02} - {:02}h{:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
        );
    }
    #[cfg(not(windows))]
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = secs / 86_400;
        let rem = secs % 86_400;
        // Civil date from days since 1970-01-01 (UTC fallback).
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        return format!(
            "{:04}/{:02}/{:02} - {:02}h{:02}:{:02}",
            y,
            m,
            d,
            rem / 3_600,
            (rem % 3_600) / 60,
            rem % 60
        );
    }
}

fn scan_presets() -> Vec<PresetEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(presets_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .map_or(false, |s| s.eq_ignore_ascii_case("xml"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = xml_elements(&text, "PresetName")
            .first()
            .map(|(_, body)| xml_unescape(body))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Preset")
                    .to_string()
            });
        let comment = xml_elements(&text, "PresetComment")
            .first()
            .map(|(_, body)| xml_unescape(body))
            .unwrap_or_default();
        let stamp = xml_elements(&text, "PresetTimeStamp")
            .first()
            .map(|(_, body)| xml_unescape(body))
            .unwrap_or_default();
        out.push(PresetEntry {
            path,
            name,
            comment,
            stamp,
        });
    }
    out.sort_by(|a, b| b.stamp.cmp(&a.stamp));
    out
}

/// Where the scene lives. The console keeps it under
/// `%APPDATA%\Audio Mixer Pro\mixer_settings.xml`. A `mixer_settings.xml`
/// next to the executable (or in the current folder) is migrated there on
/// first run, so a shortcut with any "Start in" folder still finds the same
/// file.
fn settings_file() -> PathBuf {
    // Keep user settings in one stable place regardless of how the mixer was
    // launched (double-click, shortcut, debug/release build, or a helper task
    // with a different working directory).
    let canonical = settings_canonical_path();
    if canonical.exists() {
        return canonical;
    }

    // One-time migration from the older locations. The source checkout keeps
    // its scene file two levels above target/{debug,release}; prefer that over
    // a settings file that a differently-launched build may have created
    // beside the exe.
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(app_dir) = dir.parent().and_then(|path| path.parent()) {
                candidates.push(app_dir.join("mixer_settings.xml"));
            }
            candidates.push(dir.join("mixer_settings.xml"));
        }
    }
    candidates.push(PathBuf::from("mixer_settings.xml"));

    for candidate in candidates {
        if candidate.exists() {
            if let Some(parent) = canonical.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::copy(&candidate, &canonical).is_ok() {
                return canonical;
            }
        }
    }
    canonical
}

fn settings_canonical_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("Audio Mixer Pro").join("mixer_settings.xml")
}

/// Plug-in state blob kept beside the scene, one file per input strip. The
/// header inside names the plug-in, so stale bytes are never restored into a
/// different plug-in. Slot 1 keeps the original `vst_state.bin` name so scenes
/// saved by the single-slot build still restore their plug-in settings.
fn vst_state_file(slot: usize) -> PathBuf {
    if slot == 0 {
        settings_file().with_file_name("vst_state.bin")
    } else {
        settings_file().with_file_name(format!("vst_state_{}.bin", slot + 1))
    }
}

impl MixerApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        shot_path: Option<PathBuf>,
        startup_demo: Option<usize>,
    ) -> Self {
        cc.egui_ctx
            .options_mut(|options| options.zoom_with_keyboard = true);
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = TRACK;
        visuals.faint_bg_color = PANEL_HI;
        visuals.code_bg_color = TRACK;
        visuals.override_text_color = Some(TEXT);
        visuals.hyperlink_color = BLUE;
        visuals.widgets.noninteractive.bg_fill = PANEL_HI;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        visuals.widgets.inactive.bg_fill = PANEL_HI;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
        visuals.widgets.inactive.rounding = egui::Rounding::same(3.0);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(45, 52, 63);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, AMBER.gamma_multiply(0.8));
        visuals.widgets.hovered.rounding = egui::Rounding::same(3.0);
        visuals.widgets.active.bg_fill = AMBER.gamma_multiply(0.30);
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, AMBER);
        visuals.widgets.active.rounding = egui::Rounding::same(3.0);
        visuals.widgets.open.bg_fill = PANEL_HI;
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, LINE);
        visuals.widgets.open.rounding = egui::Rounding::same(3.0);
        visuals.selection.bg_fill = AMBER.gamma_multiply(0.55);
        visuals.selection.stroke = Stroke::new(1.0, TEXT);
        visuals.window_rounding = egui::Rounding::same(6.0);
        visuals.window_stroke = Stroke::new(1.0, LINE);
        visuals.window_shadow = egui::epaint::Shadow {
            offset: Vec2::new(0.0, 6.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_black_alpha(120),
        };
        visuals.menu_rounding = egui::Rounding::same(4.0);
        // The console is a fixed dark surface; do not let the OS light theme
        // recolour egui windows and dialogs at runtime.
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        cc.egui_ctx.set_visuals(visuals);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = Vec2::new(6.0, 5.0);
        style.spacing.button_padding = Vec2::new(8.0, 3.0);
        style.spacing.window_margin = egui::Margin::same(10.0);
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(11.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(11.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(9.0));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(9.0));
        cc.egui_ctx.set_style(style);

        // The engine object exists immediately but opens no device until the
        // first painted frame (see StartupStep). `--safe-mode` skips audio
        // entirely so a wedged device graph can still be opened and fixed.
        let safe_mode =
            std::env::args().any(|arg| arg == "--safe-mode") || std::env::var_os("AMP_SAFE").is_some();
        diag_log(if safe_mode {
            "startup: safe mode"
        } else {
            "startup: constructing window"
        });
        let audio = AudioEngine::idle();
        let vst_chains = audio.vsts();
        let sample_rate = audio.sample_rate.max(8_000);
        let audio_status = if safe_mode {
            "Safe mode: audio disabled".to_string()
        } else {
            "Starting audio engine".to_string()
        };

        let strips: Vec<Strip> = (0..MAX_STRIPS).map(Strip::new).collect();
        let captures: Vec<Option<StripInput>> = (0..MAX_STRIPS).map(|_| None).collect();

        let mut app = MixerApp {
            strips,
            active_strips: BASE_STRIPS,
            strip_bank: 0,
            pending_late_virtual_adds: 0,
            pending_auto_assign: Vec::new(),
            buses: (0..audio::MAX_BUSES).map(Bus::new).collect(),
            captures,
            tape: Tape::new(),
            audio,
            sample_rate,
            buffer: VAIO_BUFFER_DEFAULT,
            main_buffer: 512,
            dev_mme: DEV_MME_DEFAULT,
            dev_wdm: DEV_WDM_DEFAULT,
            dev_ks: DEV_KS_DEFAULT,
            dev_asio: 0,
            dev_srasio: false,
            bus_latency_ms: [0.0; 3],
            input_types: [DEVICE_TYPE_WDM; 3],
            output_types: [DEVICE_TYPE_WDM; audio::MAX_BUSES],
            output_patch: [[0; 8]; audio::MAX_BUSES],
            engine_mode: 0,
            limit_remote: false,
            monitor_on_sel: false,
            lock_gui: false,
            help_caption: true,
            menu_open: false,
            menu_ignore_click: false,
            show_system: false,
            show_vc: false,
            vc: crate::vc::VirtualCables::new(),
            vc_cfg: virtual_cables::Config::import_legacy().unwrap_or_default(),
            show_about: false,
            show_audio_stats: false,
            duck: Ducking::default(),
            show_ducking: false,
            bus_rename: None,
            bus_rename_text: String::new(),
            bus_rename_focus: false,
            window_size: None,
            window_pos: None,
            window_maximized: false,
            ui_zoom: 1.0,
            streamer_mode: false,
            streamer_speed: 7.0,
            pending_maximize: false,
            mixer_hwnd: 0,
            open_editor_once: std::env::args()
                .position(|a| a == "--open-vst-editor")
                .and_then(|pos| std::env::args().nth(pos + 1))
                .and_then(|value| value.parse::<usize>().ok()),
            vst_log: std::env::args().any(|a| a == "--vst-log"),
            vst_log_clock: 0.0,
            vst_shot: std::env::args()
                .position(|a| a == "--vst-shot")
                .and_then(|pos| std::env::args().nth(pos + 1)),
            show_vban: false,
            vban: vban::VbanHub::new(),
            vban_enabled: false,
            vban_port: vban::DEFAULT_PORT,
            vban_quality: 1,
            vban_target_strip: 3,
            vban_out: Vec::new(),
            show_tape_opts: false,
            show_presets: false,
            preset_name: String::new(),
            preset_comment: String::new(),
            preset_entries: Vec::new(),
            preset_status: String::new(),
            preset_active: "DEFAULT".to_string(),
            show_preset_on_startup: false,
            show_info: None,
            apps: sessions::AppSessions::new(),
            apps_rows: HashMap::new(),
            app_icons: HashMap::new(),
            apps_open: [true; MAX_STRIPS],
            apps_scroll: [0; MAX_STRIPS],
            apps_targets: Vec::new(),
            vsts: vst_chains.into_iter().map(VstSlot::new).collect(),
            vst_panel: None,
            vst_scan: None,
            vst_scanning: false,
            vst_scan_results: Vec::new(),
            vst_scan_errors: Vec::new(),
            vst_status: String::new(),
            vst_filter: String::new(),
            auto_exit: std::env::args()
                .position(|a| a == "--exit-after")
                .and_then(|pos| std::env::args().nth(pos + 1))
                .and_then(|value| value.parse::<f64>().ok()),
            bus_eq_open: None,
            picker: None,
            loop_warned: [false; audio::MAX_BUSES],
            label_edit: None,
            label_buf: String::new(),
            strip_clip: None,
            tape_picker: false,
            tape_error: None,
            tape_info: None,
            tape_recent: Vec::new(),
            tape_path_input: String::new(),
            status: audio_status,
            status_until: 12.0,
            time: 0.0,
            frame_no: 0,
            shot_path,
            shot_requested: false,
            first_frame: true,
            startup_demo,
            startup_step: if safe_mode {
                StartupStep::Done
            } else {
                StartupStep::Engine
            },
            startup_wait: 0.0,
            startup_settle: 0.0,
            safe_mode,
            // FUN_00543160 resets AutoRestart and AutoRestartAll to 0.
            auto_restart_a1: false,
            auto_restart_all: false,
            load_on_startup: true,
            save_on_exit: true,
            poll_clock: 0.0,
            devices_probe: None,
            inputs_probe: None,
            devices_probe_at: 0.0,
            inputs_probe_at: 0.0,
            last_inputs: Vec::new(),
            inputs_applied: false,
            capture_retry_at: [0.0; audio::MAX_STRIPS],
            capture_backoff: [5.0; audio::MAX_STRIPS],
            capture_jobs: Vec::new(),
            meter_log: std::env::args().any(|arg| arg == "--meter-log"),
            meter_clock: 0.0,
            a1_reopen_tries: 0,
            restart_ticks: 0,
            last_outputs: Vec::new(),
            driver_mode: false,
            vc_was_attached: false,
        };

        // Register the capture streams that opened at startup with the output
        // stream, so their A1 buttons are already wired when the console
        // appears.
        let attached: Vec<(usize, _)> = app
            .captures
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().map(|cap| (i, cap.shared())))
            .collect();
        for (i, shared) in attached {
            app.audio.attach_input(i, shared);
        }
        // The original reads the last scene before the first paint
        // (FUN_0052f0b0 calls FUN_0052ec00 on LastSettings or VMB_StartupFile).
        app.read_persistence_flags();
        let has_saved_scene = settings_file().exists() || Path::new("mixer_settings.txt").exists();
        if app.load_on_startup && has_saved_scene {
            // Parse the scene now (cheap, pure state) but do NOT open its
            // devices yet: that is deferred until the cables have attached so
            // no capture is opened while its USB endpoint is being replugged.
            app.load_settings_parse(false);
        }
        app.audio.set_strip_count(app.active_strips);
        app.strip_bank = 0;
        if app.show_preset_on_startup {
            app.open_presets();
        }
        diag_log("startup: scene parsed");
        // Diagnostic: play the demo into both virtual buses so the cable
        // capture endpoints can be measured from another process.
        if std::env::args().any(|a| a == "--bus-demo") {
            app.tape.route = [false, false, false, true, true, false];
            app.select_demo(DemoKind::Track1);
            app.on_transport(Transport::Play);
        }
        // Diagnostic: add virtual input strips at startup
        // (`--add-virtual-inputs N`, up to the strip limit).
        if let Some(pos) = std::env::args().position(|a| a == "--add-virtual-inputs") {
            let count = std::env::args()
                .nth(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
                .min(MAX_STRIPS - BASE_STRIPS);
            for _ in 0..count {
                app.add_virtual_input();
            }
            app.picker = None;
        }
        if let Some(pos) = std::env::args().position(|a| a == "--add-virtual-inputs-late") {
            if let Some(count) = std::env::args()
                .nth(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
            {
                app.pending_late_virtual_adds = count.min(MAX_STRIPS - BASE_STRIPS);
            }
        }
        if let Some(pos) = std::env::args().position(|a| a == "--remove-virtual-inputs") {
            let count = std::env::args()
                .nth(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            for _ in 0..count {
                app.remove_last_virtual_input();
            }
            app.picker = None;
        }
        // Diagnostic: open a strip bank at startup (`--bank N`, 1-based).
        if let Some(pos) = std::env::args().position(|a| a == "--bank") {
            if let Some(bank) = std::env::args()
                .nth(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
            {
                app.strip_bank = bank.saturating_sub(1);
            }
        }
        // Diagnostic used while checking the virtual-input device list: start
        // with one strip's device picker open (`--picker 3` is Virtual 1).
        if let Some(pos) = std::env::args().position(|a| a == "--picker") {
            if let Some(i) = std::env::args()
                .nth(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
            {
                if i < MAX_STRIPS {
                    app.picker = Some(Picker::Strip(i));
                }
            }
        }
        // Diagnostic for the preset-scene window (`--presets`).
        if std::env::args().any(|a| a == "--presets") {
            app.open_presets();
        }
        // Diagnostic for the virtual-cables window (`--cables`).
        if std::env::args().any(|a| a == "--cables") {
            app.show_vc = true;
        }
        if std::env::args().any(|a| a == "--audio-stats") {
            app.show_audio_stats = true;
        }
        if std::env::args().any(|a| a == "--menu") {
            app.menu_open = true;
        }
        if std::env::args().any(|a| a == "--vban") {
            app.show_vban = true;
        }
        if std::env::args().any(|a| a == "--ducking") {
            app.show_ducking = true;
        }
        // Headless check of the scene writer/reader used by the build.
        if std::env::args().any(|a| a == "--load-settings") {
            app.load_settings(true);
        }
        if std::env::args().any(|a| a == "--save-settings") {
            app.save_settings();
        }
        // Headless round-trip of the preset writer/reader added for the
        // Preset Scene window: distinct gain -> save -> mutate -> load.
        if std::env::args().any(|a| a == "--preset-selftest") {
            app.strips[0].gain_db = 3.25;
            app.preset_name = "Self Test".to_string();
            app.preset_comment = "round trip".to_string();
            match app.save_preset() {
                Ok(path) => {
                    app.strips[0].gain_db = -6.0;
                    app.load_preset(&path);
                    let ok = (app.strips[0].gain_db - 3.25).abs() < 0.001 && path.exists();
                    println!(
                        "preset_selftest path={} gain={:.2} pass={}",
                        path.display(),
                        app.strips[0].gain_db,
                        ok
                    );
                    let _ = std::fs::remove_file(&path);
                    std::process::exit(if ok { 0 } else { 2 });
                }
                Err(e) => {
                    eprintln!("preset_selftest failed: {e}");
                    std::process::exit(2);
                }
            }
        }
        // Cables, capture devices and VSTs are opened by advance_startup once
        // the window is up. A screenshot run has no human to wait for, so it
        // still runs the whole sequence before the first frame.
        if app.shot_path.is_some() {
            app.run_startup_blocking();
        }
        // Diagnostics used while checking the VST panels: open one, optionally
        // with a plug-in already loaded. Slots are 1-5 (strip order). Run after
        // the scene restore so a diagnostic load is not immediately replaced.
        let cli: Vec<String> = std::env::args().collect();
        if let Some(pos) = cli.iter().position(|a| a == "--vst-load") {
            if let Some(path) = cli.get(pos + 1) {
                app.load_vst(0, Path::new(path), false);
            }
        }
        // Repeatable: --vst-load-slot <1-5> <path> ... for a multi-slot check.
        for (pos, arg) in cli.iter().enumerate() {
            if arg != "--vst-load-slot" {
                continue;
            }
            let slot = cli
                .get(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, MAX_STRIPS)
                - 1;
            if let Some(path) = cli.get(pos + 2) {
                app.load_vst(slot, Path::new(path), false);
            }
        }
        let optional_slot = |flag: &str| -> Option<usize> {
            let pos = cli.iter().position(|a| a == flag)?;
            let slot = cli
                .get(pos + 1)
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, MAX_STRIPS);
            Some(slot - 1)
        };
        if let Some(slot) = optional_slot("--vst-open") {
            app.open_vst_editor(slot);
        }
        if let Some(slot) = optional_slot("--vst-panel") {
            app.vst_panel = Some(slot);
            if app.vst_scan_results.is_empty() && !app.vst_scanning {
                app.start_vst_scan();
            }
        }
        // Diagnostic for the persistence round-trip: save the scene (including
        // VstPlugin and vst_state.bin) after a diagnostic plug-in load.
        if std::env::args().any(|a| a == "--vst-save-settings") {
            app.save_settings();
        }
        app
    }

    /// The strip's own label, used in the selector and in toasts.
    /// Rough end-to-end latency of the insert path, plus the breakdown shown on
    /// hover. The output figure is what was asked of the device; WASAPI shared
    /// mode may round it up to its engine period.
    fn latency_report(&self) -> (f64, String) {
        let sr = self.sample_rate.max(1) as f64;
        // Measured backlog when a strip is live; the configured target only
        // stands in when nothing is routed.
        let backlog_frames = self.audio.strip_backlog_frames();
        let input_ms = if backlog_frames > 0 {
            backlog_frames as f64 / sr * 1000.0
        } else {
            audio::INPUT_TARGET_MS as f64
        };
        let block_ms = self.main_buffer as f64 / sr * 1000.0;
        let output_ms = self.main_buffer as f64 / sr * 1000.0;
        let mut vst_ms = 0.0;
        let mut vst_count = 0usize;
        for slot in self.vsts.iter().take(self.active_strips) {
            let (in_latency, out_latency) = slot.chain.latency();
            let samples = in_latency.max(out_latency) as f64;
            if samples > 0.0 {
                vst_ms += samples / sr * 1000.0;
                vst_count += 1;
            }
        }
        let total = input_ms + block_ms + output_ms + vst_ms;
        let detail = format!(
            "Estimated insert latency\ninput backlog {input_ms:.1} ms ({backlog_frames} frames, measured)\nmixer block {block_ms:.1} ms ({} frames)\noutput request {output_ms:.1} ms ({} frames, shared mode may round it up)\nplug-in latency {vst_ms:.1} ms across {vst_count} insert(s), not compensated\ntotal {total:.1} ms\n\nA CPU spike shows up in the input backlog and is dropped back to the target\nonce it passes it, so a stall does not leave the strip permanently behind.",
            self.main_buffer, self.main_buffer
        );
        (total, detail)
    }

    fn strip_label(&self, slot: usize) -> String {
        self.strips
            .get(slot)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("INPUT {}", slot + 1))
    }

    /// Load a VST3 plug-in into one strip's insert. `restore_state` is used on
    /// startup when the saved scene names the same plug-in; a different
    /// plug-in must never receive another plug-in's state blob.
    fn load_vst(&mut self, slot: usize, path: &Path, restore_state: bool) {
        if slot >= self.vsts.len() {
            return;
        }
        let label = self.strip_label(slot);
        let previous = self.vsts[slot].path.clone();
        let wanted_bypass = self.vsts[slot].bypass;
        match self.vsts[slot]
            .chain
            .load(path, self.sample_rate as f64, vst::VST_MAX_BLOCK)
        {
            Ok(info) => {
                let state = &mut self.vsts[slot];
                state.path = path.display().to_string();
                if previous != state.path {
                    // A newly chosen plug-in starts in its own default state.
                    state.bypass = false;
                }
                state.chain.set_bypass(state.bypass);
                state.info = Some(info.clone());
                if restore_state {
                    let _ = state.chain.load_state_from(&vst_state_file(slot));
                }
                state.error = state.chain.error();
                self.vst_status = format!(
                    "{} by {} - {} input, {} output",
                    info.name, info.vendor, info.audio_inputs, info.audio_outputs
                );
                diag_log(format!(
                    "vst[{slot}] loaded '{}' ({})",
                    info.name,
                    info.format
                ));
                self.toast(format!("VST loaded on {label}: {}", info.name));
            }
            Err(err) => {
                // The old plug-in (if one was running) is still in the slot:
                // a failed replacement must not look like it removed it.
                let state = &mut self.vsts[slot];
                if !state.chain.has_plugin() {
                    state.info = None;
                }
                state.error = Some(err.clone());
                state.bypass = wanted_bypass;
                self.toast(format!("VST on {label}: {err}"));
            }
        }
    }

    /// Restore every strip's saved plug-in. Called once at startup and after
    /// the audio engine is rebuilt.
    fn restore_vst(&mut self, announce: bool) {
        for slot in 0..self.active_strips.min(self.vsts.len()) {
            let path_text = self.vsts[slot].path.trim().to_string();
            if path_text.is_empty() {
                if self.vsts[slot].chain.has_plugin() {
                    self.vsts[slot].chain.close_editor();
                    self.vsts[slot].chain.unload();
                    self.vsts[slot].info = None;
                    self.vsts[slot].bypass = false;
                    self.vsts[slot].error = None;
                }
                continue;
            }
            let label = self.strip_label(slot);
            // CLAP libraries can hold several plug-ins, so the stored path may
            // carry a `#plugin-id` suffix; the file itself is what must exist.
            let file_part = path_text.split('#').next().unwrap_or(&path_text).to_string();
            let path = PathBuf::from(&path_text);
            if !Path::new(&file_part).exists() {
                self.vsts[slot].error = Some(format!(
                    "The selected plug-in is no longer at {}",
                    file_part
                ));
                self.vsts[slot].info = None;
                continue;
            }
            self.load_vst(slot, &path, true);
            if announce {
                if let Some(info) = &self.vsts[slot].info {
                    self.toast(format!("VST restored on {label}: {}", info.name));
                }
            }
        }
    }

    fn remove_vst(&mut self, slot: usize) {
        if slot >= self.vsts.len() {
            return;
        }
        let label = self.strip_label(slot);
        let state = &mut self.vsts[slot];
        state.chain.close_editor();
        state.chain.unload();
        state.path.clear();
        state.info = None;
        state.bypass = false;
        state.error = None;
        self.vst_status = "No plug-in".to_string();
        let _ = std::fs::remove_file(vst_state_file(slot));
        self.toast(format!("VST plug-in removed from {label}"));
    }

    /// Open (or bring back) one strip's plug-in's own window. The mixer itself
    /// never draws the plug-in's settings, so this is the only way to edit them.
    /// Open the plug-in chooser for one strip. Used by the header chip and by
    /// the insert panel when the strip has no plug-in yet.
    fn open_vst_chooser(&mut self, slot: usize) {
        if slot >= self.vsts.len() {
            return;
        }
        self.vst_panel = Some(slot);
        if self.vsts[slot].info.is_none() && self.vsts[slot].chain.has_plugin() {
            self.vsts[slot].info = self.vsts[slot].chain.info();
        }
        if self.vst_scan_results.is_empty() && !self.vst_scanning {
            self.start_vst_scan();
        }
    }

    fn open_vst_editor(&mut self, slot: usize) {
        if slot >= self.vsts.len() {
            return;
        }
        let label = self.strip_label(slot);
        match self.vsts[slot].chain.open_editor(self.mixer_hwnd) {
            Ok(()) => {
                if let Some(info) = &self.vsts[slot].info {
                    self.toast(format!("Opened {label} plug-in window: {}", info.name));
                }
                self.vsts[slot].error = None;
            }
            Err(err) => {
                diag_log(format!("vst[{slot}] load failed: {err}"));
                self.vsts[slot].error = Some(err.clone());
                self.toast(format!("VST on {label}: {err}"));
            }
        }
    }

    fn start_vst_scan(&mut self) {
        if self.vst_scanning {
            return;
        }
        self.vst_scanning = true;
        self.vst_scan = Some(vst::start_scan(Vec::new()));
        self.vst_status = "Scanning the standard VST3 folders...".to_string();
    }

    fn poll_vst_scan(&mut self) {
        let Some(receiver) = &self.vst_scan else {
            return;
        };
        match receiver.try_recv() {
            Ok(outcome) => {
                self.vst_scan_results = outcome.plugins;
                self.vst_scan_errors = outcome.errors;
                self.vst_scanning = false;
                self.vst_scan = None;
                self.vst_status = format!(
                    "Found {} plug-in(s) in the standard VST3 folders",
                    self.vst_scan_results.len()
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.vst_scanning = false;
                self.vst_scan = None;
                self.vst_status = "The VST3 scan stopped unexpectedly".to_string();
            }
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast_for(text, 3.5);
    }

    fn toast_for(&mut self, text: impl Into<String>, seconds: f64) {
        self.status = text.into();
        self.status_until = self.time + seconds;
    }

    /// Say out loud what a routing button just did, because a route that is
    /// lit but has no device behind it is the one case that looks like a bug.
    fn announce_route(&mut self, strip: usize, bus: usize, on: bool) {
        let bus_name = ["A1", "A2", "A3", "B1", "B2", "B3"][bus.min(audio::MAX_BUSES - 1)];
        let name = self.strips[strip].name.clone();
        if !on {
            self.toast(format!("{name} is no longer routed to {bus_name}"));
            return;
        }
        if strip < 3 {
            if self.captures.get(strip).and_then(|c| c.as_ref()).is_none() {
                self.toast(format!(
                    "{name} -> {bus_name} on, but this strip has no input device. Click its name to choose one."
                ));
            } else {
                self.toast(format!(
                    "{name} routed to {bus_name}{}",
                    if bus == 0 { " - it should be heard now" } else { "" }
                ));
            }
        } else if strip == 3 {
            self.toast(format!(
                "Virtual input 1 routed to {bus_name} (fed by the tape deck)"
            ));
        } else {
            self.toast(format!("{name} routed to {bus_name}"));
        }
    }

    fn push_recent(&mut self, path: &Path) {
        self.tape_recent.retain(|p| p != path);
        self.tape_recent.insert(0, path.to_path_buf());
        self.tape_recent.truncate(6);
    }

    fn select_demo(&mut self, kind: DemoKind) {
        self.audio.load_demo(kind);
        self.tape.demo = Some(kind);
        self.tape.path.clear();
        self.tape.file = kind.file_name().to_string();
        self.tape.len = audio::DEMO_LEN_SECS;
        self.tape.pos = 0.0;
        self.tape.playing = false;
        self.tape.recording = false;
        self.tape_error = None;
        self.tape_info = Some("built-in generated track".to_string());
        if self.audio.available {
            self.toast(format!("{} selected - press Play", kind.file_name()));
        } else {
            self.toast("No audio output device - the tape deck cannot be heard");
        }
    }

    /// Load a file the person picked. Returns true when the tape deck accepted
    /// it; on failure the reason stays visible in the picker and the status
    /// bar instead of disappearing after a moment.
    fn load_wav_file(&mut self, path: PathBuf) -> bool {
        let display = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        match self.audio.load_wav(&path) {
            Ok(info) => {
                self.tape.demo = None;
                self.tape.path = path.display().to_string();
                self.tape.file = display.clone();
                self.tape.len = info.duration.max(0.1);
                self.tape.pos = 0.0;
                self.tape.playing = false;
                self.tape.recording = false;
                self.tape_error = None;
                self.tape_info = Some(info.detail());
                self.push_recent(&path);
                if self.audio.available {
                    self.toast(format!(
                        "Loaded {} ({}) - press Play",
                        display,
                        clock(info.duration)
                    ));
                } else {
                    self.toast("No audio output device - the tape deck cannot be heard");
                }
                true
            }
            Err(err) => {
                self.tape_error = Some(format!("{display}: {err}"));
                self.tape_info = None;
                self.toast_for(format!("Could not load {display}: {err}"), 9.0);
                false
            }
        }
    }

    fn reload_tape_audio(&mut self) {
        if let Some(kind) = self.tape.demo {
            self.audio.load_demo(kind);
        } else if !self.tape.path.is_empty() && Path::new(&self.tape.path).exists() {
            let _ = self.audio.load_wav(Path::new(&self.tape.path));
        } else {
            return;
        }
        self.audio.seek(self.tape.pos);
        if self.tape.playing {
            self.audio.play();
        }
    }

    fn eject_tape(&mut self) {
        self.audio.unload();
        self.tape = Tape::new();
        self.tape_error = None;
        self.tape_info = None;
        self.toast("Cassette ejected - audio file released");
    }

    fn sync_audio_controls(&mut self) {
        self.audio.set_tape(self.tape.gain_db, self.tape.route);
        self.audio.set_loop(self.tape.loop_playback);
        self.audio.set_strip_count(self.active_strips);
        // The console has five buses; this rebuild has one physical output, so
        // the A1 card is the one whose fader, mute, mono and EQ are audible.
        let any_solo = self.strips[..self.active_strips].iter().any(|s| s.solo);
        for (i, s) in self.strips.iter().take(self.active_strips).enumerate() {
            // The route buttons are real switches in the audio callback: an
            // input strip with A1 on is mixed into the A1 output with its
            // fader, pan, mute, solo and mono settings.
            self.audio.set_strip(
                i,
                s.gain_db,
                s.pan,
                s.comp,
                s.gate,
                s.mute,
                s.solo,
                s.mono,
                s.route,
                any_solo,
            );
            self.audio.set_strip_eq(i, s.eq);
        }
        for (i, bus) in self.buses.iter().enumerate() {
            self.audio.set_bus(
                i,
                bus.gain_db,
                bus.mute,
                bus.mono,
                bus.eq_on,
                [
                    bus.eq[0].gain,
                    bus.eq[1].gain,
                    bus.eq[2].gain,
                    bus.eq[3].gain,
                    bus.eq[4].gain,
                    bus.eq[5].gain,
                ],
            );
        }
        // Ducking: one source keys attenuation of the selected target strips.
        // Bits outside the visible strip range are ignored.
        let targets = self.duck.targets & ((1u32 << self.active_strips.min(31)) - 1);
        self.audio.set_ducking(
            self.duck.enabled,
            self.duck.source.filter(|src| *src < self.active_strips),
            targets,
            self.duck.depth_db,
            self.duck.threshold_db,
            self.duck.attack_ms,
            self.duck.release_ms,
        );
        // B1/B2: an explicit device plays only to that device. Left unset they
        // keep the automatic cable wiring - a render stream to the driver's
        // Virtual Cable 01/02, or the direct injection into USB Cable 01/02
        // when the kernel driver is not installed.
        let auto_b1 = self.buses[3].device.trim().is_empty();
        let auto_b2 = self.buses[4].device.trim().is_empty();
        let b1 = if !auto_b1 {
            self.buses[3].device.clone()
        } else if self.driver_mode {
            cable_render_device(&self.last_outputs, "Virtual Cable 01")
        } else {
            String::new()
        };
        let b2 = if !auto_b2 {
            self.buses[4].device.clone()
        } else if self.driver_mode {
            cable_render_device(&self.last_outputs, "Virtual Cable 02")
        } else {
            String::new()
        };
        let usb1 = if auto_b1 && !self.driver_mode {
            self.vc.device(1)
        } else {
            None
        };
        let usb2 = if auto_b2 && !self.driver_mode {
            self.vc.device(2)
        } else {
            None
        };
        self.audio.set_virtual_outputs(usb1, usb2);
        let a1 = self.buses[0].device.clone();
        let a2 = self.buses[1].device.clone();
        let a3 = self.buses[2].device.clone();
        // B3 is a plain bus: it only plays to the device chosen for it.
        let b3 = self.buses[5].device.clone();
        // Outputs obey the same rule as captures: a USB cable endpoint appears
        // in enumeration while usbip is still attaching it, and opening it then
        // holds the endpoint so the attach never completes, leaving that cable
        // unusable for the rest of the session. Wait for the attach report; the
        // bus is reopened as soon as it arrives.
        let usb_ready = !self.vc_cfg.enabled || self.vc.status().attached;
        let gate = |name: String| -> String {
            if usb_ready || !is_usb_cable_endpoint(&name) {
                name
            } else {
                String::new()
            }
        };
        let a1 = gate(a1);
        let a2 = gate(a2);
        let a3 = gate(a3);
        let b1 = gate(b1);
        let b2 = gate(b2);
        let b3 = gate(b3);
        self.audio.set_bus_outputs_with(
            &self.last_outputs,
            [
                a1.as_str(),
                a2.as_str(),
                a3.as_str(),
                b1.as_str(),
                b2.as_str(),
                b3.as_str(),
            ],
        );
        self.audio.set_vban_tx(self.vban.tx_targets());
    }

    /// Re-open A1 with the values in System Settings. FUN_00527ad0 only takes
    /// the seven standard rates and a main buffer of at most 2048 samples; the
    /// exact A1 error text is returned when the device refuses the format.
    fn apply_vban_config(&mut self) {
        self.vban.configure(self.vban_enabled, self.vban_port);
        let configs = if self.vban_enabled {
            self.vban_out.clone()
        } else {
            Vec::new()
        };
        let _ = self.vban.set_senders(configs, self.audio.sample_rate);
    }

    fn apply_audio_format(&mut self) -> Result<(), String> {
        // A1 is just one output bus now; the mixer rate/buffer come from the
        // System Settings and every bus device is opened by set_bus_outputs.
        let result = self
            .audio
            .apply_format(self.sample_rate, self.main_buffer, None);
        self.sample_rate = self.audio.sample_rate.max(8_000);
        if result.is_ok() {
            self.reload_tape_audio();
            // Sender pacing/resampling follows the A1 rate.
            self.apply_vban_config();
        }
        result
    }

    /// The part of the original's menu command handler (`FUN_00543640`) that
    /// this rebuild implements. Command ids come from `FUN_0052faa0`.
    ///
    /// Case 0x55 (A1 device): switching it off also clears the all-device
    /// option. Case 0x45 (All Device): switching it on also sets A1; switching
    /// it off leaves A1 set.
    fn menu_command(&mut self, cmd: u32) {
        match cmd {
            CMD_RESTART_ENGINE => self.restart_engine(),
            CMD_AUTO_RESTART_A1 => {
                if self.auto_restart_a1 {
                    self.auto_restart_a1 = false;
                    self.auto_restart_all = false;
                    self.restart_ticks = 0;
                } else {
                    self.auto_restart_a1 = true;
                }
            }
            CMD_AUTO_RESTART_ALL => {
                if self.auto_restart_all {
                    self.auto_restart_all = false;
                } else {
                    self.auto_restart_all = true;
                    self.auto_restart_a1 = true;
                }
            }
            _ => {}
        }
    }

    /// FUN_0054b730 calls FUN_00527ad0(1, 1) from the one-second watchdog.
    /// Its second parameter is stored at console[0x1f00] as the "auto" flag;
    /// with it set, an A1 that refuses to open sets the engine error flag but
    /// shows no message box. The rebuild retries quietly and only reports a
    /// success.
    fn silent_restart(&mut self) {
        if self.apply_audio_format().is_ok() {
            self.toast("Audio engine restarted automatically");
        }
    }

    /// Kick the capture-device probe (the UI thread must never enumerate).
    fn probe_inputs_now(&mut self) {
        if self.inputs_probe.is_some() || self.inputs_probe_at > 0.0 && self.time - self.inputs_probe_at < 1.0 {
            return;
        }
        self.inputs_probe_at = self.time;
        let (sender, receiver) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("input-probe".to_string())
            .spawn(move || {
                let _ = sender.send(crate::audio::InputCapture::devices());
            })
            .is_ok()
        {
            self.inputs_probe = Some(receiver);
        }
    }

    /// Kick the device probe immediately if one is not already running. Used
    /// by start-up, which must not wait a full watchdog tick for the list.
    fn probe_devices_now(&mut self) {
        if self.devices_probe.is_some() || self.devices_probe_at > 0.0 && self.time - self.devices_probe_at < 1.0 {
            return;
        }
        self.devices_probe_at = self.time;
        let (sender, receiver) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("device-probe".to_string())
            .spawn(move || {
                let _ = sender.send(AudioEngine::output_devices());
            })
            .is_ok()
        {
            self.devices_probe = Some(receiver);
        }
        if self.inputs_probe.is_none() {
            let (sender, receiver) = std::sync::mpsc::channel();
            if std::thread::Builder::new()
                .name("input-probe".to_string())
                .spawn(move || {
                    let _ = sender.send(crate::audio::InputCapture::devices());
                })
                .is_ok()
            {
                self.inputs_probe = Some(receiver);
            }
        }
    }

    /// Open a capture device on a worker so a wedged endpoint cannot freeze
    /// the console; the result is applied on a later frame.
    fn spawn_capture_open(&mut self, slot: usize, name: String) {
        let (sender, receiver) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("capture-open".to_string())
            .spawn(move || {
                let _ = sender.send(audio::InputCapture::open(Some(&name)));
            })
            .is_ok()
        {
            self.capture_jobs.push((slot, receiver));
        }
    }

    /// Apply capture opens that finished since the last frame.
    fn poll_capture_jobs(&mut self) {
        let mut done = Vec::new();
        self.capture_jobs.retain(|(slot, receiver)| match receiver.try_recv() {
            Ok(capture) => {
                done.push((*slot, capture));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        for (slot, capture) in done {
            self.apply_opened_capture(slot, capture);
        }
    }

    /// Shared post-open handling for the picker and the watchdog.
    fn apply_opened_capture(&mut self, i: usize, capture: audio::InputCapture) {
        if capture.available {
            self.vban.unbind(i);
            self.strips[i].device = capture.device.clone();
            self.audio.attach_input(i, capture.shared());
            if i < 3 && !self.strips[i].route.iter().any(|r| *r) {
                self.strips[i].route[0] = true;
            }
            let device = capture.device.clone();
            self.captures[i] = Some(StripInput::Cpal(capture));
            self.strips[i].tape_feed = false;
            self.capture_backoff[i] = 5.0;
            self.capture_retry_at[i] = 0.0;
            self.toast(format!(
                "Listening to {device} - its A1-A3/B1-B2 buttons now route it"
            ));
        } else {
            let reason = capture.status.clone();
            self.vban.unbind(i);
            self.captures[i] = None;
            self.audio.detach_input(i);
            self.strips[i].device = capture.device.clone();
            self.strips[i].meter = 0.0;
            self.strips[i].peak = 0.0;
            self.toast(format!("{reason}"));
        }
    }

    /// Drain the capture-device probe. Never enumerates on the UI thread.
    fn poll_input_devices(&mut self) {
        let Some(probe) = self.inputs_probe.take() else {
            return;
        };
        match probe.try_recv() {
            Ok(list) => {
                if list.len() != self.last_inputs.len() {
                    diag_log(format!("inputs probe: {} device(s): {}", list.len(), list.iter().take(8).cloned().collect::<Vec<_>>().join(" | ")));
                }
                self.last_inputs = list;
                // Start-up may have run before the probe answered, in which
                // case the saved captures were skipped; apply them now.
                if !self.inputs_applied && !self.last_inputs.is_empty() {
                    self.inputs_applied = true;
                    self.apply_saved_inputs();
                }
                // Self-heal: a strip whose saved device was missing at start-up
                // (a cable endpoint still being attached, a device plugged in
                // later) is opened as soon as the probe actually lists it. This
                // is what the picker used to be needed for.
                for i in 0..self.active_strips.min(self.captures.len()) {
                    if self.captures[i].is_some() {
                        continue;
                    }
                    let name = self.strips[i].device.clone();
                    let trimmed = name.trim();
                    if trimmed.is_empty()
                        || trimmed == "No input"
                        || trimmed == "No device"
                        || trimmed == "Virtual 1 (VAIO)"
                        || trimmed == "Virtual 2 (AUX)"
                        || trimmed.starts_with("VBAN: ")
                    {
                        continue;
                    }
                    if !self
                        .last_inputs
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(trimmed))
                    {
                        continue;
                    }
                    // A cable endpoint shows up in enumeration while usbip is
                    // still attaching it. Opening it then yields a stream that
                    // runs but stays permanently silent, and it holds the
                    // endpoint so the attach cannot finish (Chrome then cannot
                    // even see the cable). Wait until the attach reports done.
                    if self.vc_cfg.enabled
                        && is_usb_cable_endpoint(&name)
                        && !self.vc.status().attached
                    {
                        continue;
                    }
                    if self.capture_jobs.iter().any(|(slot, _)| *slot == i) {
                        continue;
                    }
                    self.spawn_capture_open(i, name);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.inputs_probe = Some(probe),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// One-second device watchdog, following FUN_0054b730: it returns early
    /// until 1000 ms have passed, compares the current device set with the
    /// previous one, and after five consecutive ticks with a bad or moved A1
    /// stream calls FUN_00527ad0(1, 1) without a dialog.
    fn poll_audio_devices(&mut self, dt: f64) {
        if self.safe_mode {
            return;
        }
        // Enumeration calls into the audio service and can block for minutes
        // when a device is wedged, so it runs off the UI thread. A probe still
        // in flight means the service is not answering: skip the rest of this
        // tick instead of stacking more audio calls behind it.
        if let Some(probe) = self.devices_probe.take() {
            match probe.try_recv() {
                Ok(devices) => self.handle_device_list(devices),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.devices_probe = Some(probe);
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        self.poll_clock += dt;
        if self.poll_clock < 1.0 {
            return;
        }
        self.poll_clock = 0.0;
        let (sender, receiver) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("device-probe".to_string())
            .spawn(move || {
                let _ = sender.send(AudioEngine::output_devices());
            })
            .is_ok()
        {
            self.devices_probe = Some(receiver);
        }
        // Refresh the capture list on the same cadence: a cable attached after
        // start-up (or a device plugged in later) only becomes visible here, and
        // the strips depend on this list to reopen their saved device.
        self.probe_inputs_now();
    }

    /// React to a list that arrived from the probe thread.
    fn handle_device_list(&mut self, devices: Vec<String>) {
        let list_changed = !self.last_outputs.is_empty() && devices != self.last_outputs;
        let recovered = self.last_outputs.is_empty() && !devices.is_empty();
        self.driver_mode = driver_cables_present_from(&devices);
        self.last_outputs = devices;
        // Start-up skips captures while the audio service is not answering.
        // The first list that arrives means it is back, so pick them up now.
        if recovered && self.startup_step == StartupStep::Done && self.audio.available {
            diag_log("startup: audio service recovered, opening capture devices");
            self.open_default_input();
            self.apply_saved_inputs();
        }

        if self.audio.available && !list_changed {
            self.restart_ticks = 0;
        } else {
            self.restart_ticks += 1;
            if self.restart_ticks > 4 {
                self.restart_ticks = 0;
                if self.auto_restart_a1 || self.auto_restart_all {
                    self.silent_restart();
                }
            }
        }

        if self.auto_restart_all && !self.last_inputs.is_empty() {
            // Only act on a list that came from the probe thread. An empty list
            // means the audio service is not answering, and every attempt to
            // enumerate or open would block the UI thread behind it.
            for i in 0..self.active_strips.min(self.captures.len()) {
                let Some(cap) = self.captures[i].as_ref() else {
                    continue;
                };
                if matches!(cap, StripInput::Vban(_)) {
                    continue;
                }
                let gone = !self.last_inputs.iter().any(|d| d == cap.device());
                if !gone {
                    continue;
                }
                if self.time < self.capture_retry_at[i] {
                    continue;
                }
                let name = cap.device().to_string();
                // Opening is a blocking audio-service call: run it on a worker
                // and apply the result when it lands.
                self.spawn_capture_open(i, name.clone());
                let backoff = self.capture_backoff[i].min(60.0);
                self.capture_retry_at[i] = self.time + backoff;
                self.capture_backoff[i] = (backoff * 2.0).min(60.0);
            }
        }

        // The USB cable server starts after the scene is restored, so a saved
        // virtual-strip capture can be opened against a stale endpoint. When
        // the cables report attached, drop those two streams so the retry
        // below opens them fresh.
        let vc_attached = self.vc.status().attached;
        if vc_attached && !self.vc_was_attached {
            for i in 0..self.active_strips {
                if matches!(self.captures[i], Some(StripInput::Vban(_))) {
                    continue;
                }
                if cable_label(&self.strips[i].device).is_some() {
                    self.audio.detach_input(i);
                    self.captures[i] = None;
                }
            }
        }
        if vc_attached && self.pending_late_virtual_adds > 0 {
            let count = self
                .pending_late_virtual_adds
                .min(MAX_STRIPS - self.active_strips);
            for _ in 0..count {
                self.add_virtual_input();
            }
            self.pending_late_virtual_adds -= count;
            self.picker = None;
        }
        self.vc_was_attached = vc_attached;

        // Opening a capture endpoint is another call into the audio service, so
        // it only happens once the engine is actually up.
        if self.audio.available {
            // Finish auto-assigning extra inputs that were added before Windows
            // enumerated their virtual cable.
            self.retry_pending_auto_assign();

            // A virtual cable can finish attaching a moment after the mixer
            // starts, so the first restore attempt may find no endpoint. Keep
            // retrying the saved capture devices quietly; otherwise the strip
            // stays silent until the person reselects it.
            self.apply_saved_inputs();
        }
    }

    fn restart_engine(&mut self) {
        for s in &mut self.strips {
            s.meter = 0.0;
            s.peak = 0.0;
            s.hold = 0.0;
        }
        for b in &mut self.buses {
            b.meter = 0.0;
            b.peak = 0.0;
            b.hold = 0.0;
        }
        // Keep the selected A1 format: command 0x50 in FUN_00543640 restarts
        // StartAudioStream with the current settings.
        let wanted_rate = self.sample_rate;
        self.audio = AudioEngine::new();
        // The new engine owns five fresh VST slots, so put the saved plug-ins
        // back once the new output stream (and its sample rate) exists. Every
        // plug-in's own window must close before its chain goes away.
        for slot in &mut self.vsts {
            slot.chain.close_editor();
        }
        let chains = self.audio.vsts();
        for (slot, chain) in self.vsts.iter_mut().zip(chains.into_iter()) {
            slot.chain = chain;
            slot.info = None;
        }
        self.sample_rate = wanted_rate;
        match self.apply_audio_format() {
            Ok(()) => self.toast("Audio engine restarted"),
            Err(err) => {
                self.toast("A1 output failed - see the message");
                self.show_info = Some((
                    "A1 Output Device".to_string(),
                    format!(
                        "{err}\n\nAnother application may be holding the audio device. \
                         Quit it, then the engine reopens by itself (or press Restart Audio Engine)."
                    ),
                ));
            }
        }
        self.restore_vst(false);
    }

    /// A sample-rate change needs every plug-in re-prepared. Each slot's own
    /// state is saved first so the reload keeps the current settings.
    fn reconfigure_vst(&mut self) {
        for slot in 0..self.active_strips.min(self.vsts.len()) {
            if self.vsts[slot].path.trim().is_empty() {
                continue;
            }
            let _ = self.vsts[slot].chain.save_state_to(&vst_state_file(slot));
        }
        self.restore_vst(false);
    }

    /// Drive every meter from real audio. Hardware strips read the peak of
    /// their Windows capture stream, Virtual Input 1 reads the tape deck's
    /// material, A1 reads what actually left the output device, and A2/A3/B1/
    /// B2 read the tape level through their own routing and fader (they have
    /// no physical device in this rebuild). Motion follows FUN_00538130: a new
    /// maximum is held for 50 timer ticks (15 ms each), then the meter follows
    /// the live value.
    fn update_meters(&mut self, dt: f32) {
        let any_solo = self.strips[..self.active_strips].iter().any(|s| s.solo);
        let tape_source = self.audio.source_peak();
        for i in 0..self.active_strips {
            // A strip meters its open Windows capture endpoint. Virtual
            // Input 1 keeps the original's tape feed when it has no explicit
            // device, and Virtual Input 2 stays silent until a device is
            // chosen (the original console feeds it from its AUX device).
            let raw = self
                .captures
                .get(i)
                .and_then(|c| c.as_ref())
                .map(|c| c.peak())
                .unwrap_or_else(|| {
                    if self.strips[i].tape_feed {
                        tape_source
                    } else {
                        0.0
                    }
                });
            let s = &mut self.strips[i];
            let mut target = raw * db_to_lin(s.gain_db);
            if s.mute || (any_solo && !s.solo) {
                target = 0.0;
            }
            follow_meter(
                &mut s.meter,
                &mut s.peak,
                &mut s.hold,
                target.clamp(0.0, 4.0),
                dt,
            );
        }

        let output = self.audio.peak();
        // The virtual buses have no physical device in this rebuild. Show the
        // real level that the routed strips (and the tape deck) would send
        // through their own fader, so a strip's A2/A3/B1/B2 button visibly
        // does something even though only A1 is opened.
        let mut bus_source = [0.0f32; audio::MAX_BUSES];
        for bi in 1..audio::MAX_BUSES {
            let mut sum = 0.0f32;
            for (si, s) in self.strips.iter().take(self.active_strips).enumerate() {
                if !s.route[bi] || s.mute || (any_solo && !s.solo) {
                    continue;
                }
                let raw = self
                    .captures
                    .get(si)
                    .and_then(|c| c.as_ref())
                    .map(|c| c.peak())
                    .unwrap_or_else(|| if s.tape_feed { tape_source } else { 0.0 });
                sum += raw * db_to_lin(s.gain_db);
            }
            if self.tape.route[bi] {
                sum += tape_source;
            }
            bus_source[bi] = sum;
        }
        let bus_peaks: [f32; audio::MAX_BUSES] =
            std::array::from_fn(|bus| self.audio.bus_peak(bus));
        for (bi, b) in self.buses.iter_mut().enumerate() {
            let mut target = if bi == 0 {
                // The A1 meter is the level measured at the real output, so it
                // includes every routed strip, the A1 fader, mute, mono, EQ and
                // soft limiter.
                output
            } else if bi >= 1 {
                // A2/A3 and B1/B2 are real bus stems now (VBAN/cable outputs).
                bus_peaks[bi]
            } else {
                bus_source[bi] * db_to_lin(b.gain_db)
            };
            if b.mute {
                target = 0.0;
            }
            follow_meter(
                &mut b.meter,
                &mut b.peak,
                &mut b.hold,
                target.clamp(0.0, 4.0),
                dt,
            );
        }
    }


    fn scene_document(&self) -> String {
        // FUN_00529c80 opens the document, then FUN_00528b40 writes the scene
        // block. Attribute names, order and format strings are the original's.
        let mut text = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<AudioMixerProSettings>\n",
        );
        text.push_str("<AudioMixerProDeviceConfiguration>\n");
        // FUN_00529c80's exact OptionDev format. FUN_0052ab70 accepts
        // 128..=2048 for MME/WDM/KS, 0 or 64..=1024 for ASIO, and 0..=500 ms
        // for each bus latency; keep the stored values inside those bounds.
        text.push_str(&format!(
            "\t<OptionDev mme='{}' wdm='{}' ks='{}' asio='{}' srasio='{}' msA1='{:.2}' msA2='{:.2}' msA3='{:.2}' />\n",
            self.dev_mme.clamp(DEV_BUFFER_MIN, DEV_BUFFER_MAX),
            self.dev_wdm.clamp(DEV_BUFFER_MIN, DEV_BUFFER_MAX),
            self.dev_ks.clamp(DEV_BUFFER_MIN, DEV_BUFFER_MAX),
            self.dev_asio.clamp(0, ASIO_BUFFER_MAX),
            self.dev_srasio as u8,
            self.bus_latency_ms[0].clamp(0.0, DEV_MS_MAX),
            self.bus_latency_ms[1].clamp(0.0, DEV_MS_MAX),
            self.bus_latency_ms[2].clamp(0.0, DEV_MS_MAX)
        ));
        // FUN_00529c80 writes type only when the device name is present; an
        // empty input becomes name="-" with no type. An unassigned output also
        // carries its eight pOut patch numbers, each 0..=64.
        for i in 0..3 {
            let name = xml_escape(&self.strips[i].device);
            if self.strips[i].device.trim().is_empty() {
                text.push_str(&format!(
                    "\t<InputDev index='{}' name=\"-\" />\n",
                    i + 1
                ));
            } else {
                text.push_str(&format!(
                    "\t<InputDev index='{}' type='{}' name=\"{}\" />\n",
                    i + 1,
                    self.input_types[i],
                    name
                ));
            }
        }
        // Rebuild extension: virtual strips store their chosen Windows capture
        // endpoint in the same element (indices 4+). The original's tag-driven
        // reader only knows 1-3 and ignores these.
        for i in 3..self.active_strips {
            let name = if self.strips[i].device.trim().is_empty() {
                if i == 3 {
                    "Virtual 1 (VAIO)"
                } else if i == 4 {
                    "Virtual 2 (AUX)"
                } else {
                    "No input"
                }
            } else {
                self.strips[i].device.as_str()
            };
            text.push_str(&format!(
                "\t<InputDev index='{}' name=\"{}\" />\n",
                i + 1,
                xml_escape(name)
            ));
        }
        for i in 0..audio::MAX_BUSES {
            let name = xml_escape(&self.buses[i].device);
            if self.buses[i].device.trim().is_empty() {
                let mut patches = String::new();
                for n in 0..8 {
                    patches.push_str(&format!(
                        "pOut{}='{}' ",
                        n + 1,
                        self.output_patch[i][n].clamp(0, OUT_PATCH_MAX)
                    ));
                }
                text.push_str(&format!(
                    "\t<OutputDev index='{}' name=\"-\" {} />\n",
                    i + 1,
                    patches
                ));
            } else {
                text.push_str(&format!(
                    "\t<OutputDev index='{}' type='{}' name=\"{}\" />\n",
                    i + 1,
                    self.output_types[i],
                    name
                ));
            }
        }
        text.push_str("</AudioMixerProDeviceConfiguration>\n");
        text.push_str("<AudioMixerProParameters>\n");
        for (i, s) in self.strips.iter().enumerate().take(3) {
            text.push_str(&format!(
                "\t<LabelStrip{}>{}</LabelStrip{}>\n",
                i + 1,
                xml_escape(&s.name),
                i + 1
            ));
        }
        for (i, s) in self
            .strips
            .iter()
            .enumerate()
            .take(self.active_strips)
            .skip(3)
        {
            text.push_str(&format!(
                "\t<LabelVirtualStrip{}>{}</LabelVirtualStrip{}>\n",
                i - 2,
                xml_escape(&s.name),
                i - 2
            ));
        }
        text.push_str(&format!(
            "\t<VMOptions MonitorOnSEL='{}' SliderRelative='0' />\n",
            self.monitor_on_sel as u8
        ));
        for (i, s) in self.strips.iter().take(self.active_strips).enumerate() {
            // FUN_00528b40 bit layout: mute 0x1, solo 0x2, mono 0x4,
            // muc 0x8, prg_c 0x10, prg_g 0x20, busa 0x1000, busa2 0x2000,
            // busa3 0x4000, busb 0x10000, busb2 0x20000, vaio 0x40000000.
            let mut flags: u32 = 0;
            if s.mute {
                flags |= 0x1;
            }
            if s.solo {
                flags |= 0x2;
            }
            if s.mono {
                flags |= 0x4;
            }
            if s.mc {
                flags |= 0x8;
            }
            // FUN_00528b40 emits prg_c from bit 0x10 and prg_g from bit 0x20.
            if s.comp_prg {
                flags |= 0x10;
            }
            if s.gate_prg {
                flags |= 0x20;
            }
            if s.route[0] {
                flags |= 0x1000;
            }
            if s.route[1] {
                flags |= 0x2000;
            }
            if s.route[2] {
                flags |= 0x4000;
            }
            if s.route[3] {
                flags |= 0x10000;
            }
            if s.route[4] {
                flags |= 0x20000;
            }
            if s.route[5] {
                flags |= 0x40000;
            }
            if s.kind == StripKind::Virtual {
                flags |= 0x4000_0000;
            }
            text.push_str(&format!(
                "\t<Strip index='{}' mute='{}' vaio='{}' solo='{}' mono='{}' muc='{}' prg_c='{}' prg_g='{}' busa='{}' busa2='{}' busa3='{}' busb='{}' busb2='{}' busb3='{}' karaoke='{}' dblevel='{:.2}' dblimit='{:.2}' />\n",
                i + 1,
                flags & 1,
                (flags >> 30) & 1,
                (flags >> 1) & 1,
                (flags >> 2) & 1,
                (flags >> 3) & 1,
                (flags >> 4) & 1,
                (flags >> 5) & 1,
                (flags >> 12) & 1,
                (flags >> 13) & 1,
                (flags >> 14) & 1,
                (flags >> 16) & 1,
                (flags >> 17) & 1,
                (flags >> 18) & 1,
                s.karaoke,
                s.gain_db,
                s.limit
            ));
            text.push_str(&format!(
                "\t<StripComp index='{}' gainin='{:.2}' attack='{:.1}' release='{:.1}' knee='{:.2}' comprate='{:.3}' threshold='{:.2}' automakeup='{}' gainout='{:.2}' />\n",
                i + 1,
                COMP_GAININ_DB,
                COMP_ATTACK_MS,
                COMP_RELEASE_MS,
                COMP_KNEE,
                COMP_RATIO,
                COMP_THRESHOLD_DB,
                COMP_AUTOMAKEUP,
                COMP_GAINOUT_DB
            ));
            text.push_str(&format!(
                "\t<StripGate index='{}' thresin='{:.2}' damping='{:.2}' bpsidechain='{:.0}' attack='{:.1}' hold='{:.1}' release='{:.1}' />\n",
                i + 1,
                GATE_THRESHOLD_DB,
                GATE_DAMPING_DB,
                GATE_SIDECHAIN_PCT,
                GATE_ATTACK_MS,
                GATE_HOLD_MS,
                GATE_RELEASE_MS
            ));
            if i < 3 {
                // FUN_00528b40 hardware strip format (0x569b60): all three
                // panel pairs are absolute fields selected by paneltype.
                text.push_str(&format!(
                    "\t<Strip index='{}' paneltype='{}' Panel3Dx='{:.3}' Panel3Dy='{:.3}' ColorPanelx='{:.3}' ColorPanely='{:.3}' ModPanelx='{:.3}' ModPanely='{:.3}' audibility_c='{:.2}' audibility_g='{:.2}' />\n",
                    i + 1,
                    s.panel_type,
                    s.intelli[0],
                    s.intelli[1],
                    s.color_panel[0],
                    s.color_panel[1],
                    s.mod_panel[0],
                    s.mod_panel[1],
                    s.comp,
                    s.gate
                ));
            } else {
                text.push_str(&format!(
                    "\t<Strip index='{}' EQGain1='{:.2}' EQGain2='{:.2}' EQGain3='{:.2}' Panel3Dx='{:.3}' Panel3Dy='{:.3}' />\n",
                    i + 1, s.eq[0], s.eq[1], s.eq[2], s.intelli[0], s.intelli[1]
                ));
            }
        }
        for (i, b) in self.buses.iter().enumerate() {
            // FUN_00528b40 bus bits: mute 0x1, vaio 0x40000000, mono 0x4,
            // cross 0x200, BusMode nibble at 4, EQon 0x100, SEL 0x1c000000,
            // monitor 0x20000000.
            let mut flags: u32 = 0;
            if b.mute {
                flags |= 0x1;
            }
            if i >= 3 {
                flags |= 0x4000_0000;
            }
            if b.mono == 1 {
                flags |= 0x4;
            }
            if b.mono == 2 {
                flags |= 0x200;
            }
            flags |= ((b.mode as u32) & 0xf) << 4;
            if b.eq_on {
                flags |= 0x100;
            }
            if b.sel {
                flags |= 0x1000_0000;
            }
            if b.monitor {
                flags |= 0x2000_0000;
            }
            text.push_str(&format!(
                "\t<Bus index='{}' mute='{}' vaio='{}' mono='{}' cross='{}' BusMode='{}' EQon='{}' SEL='{}' monitor='{}' dblevel='{:.2}' />\n",
                i + 1,
                flags & 1,
                (flags >> 30) & 1,
                if b.mono == 1 { 1 } else { 0 },
                if b.mono == 2 { 1 } else { 0 },
                (flags >> 4) & 0xf,
                (flags >> 8) & 1,
                (flags >> 28) & 1,
                (flags >> 29) & 1,
                b.gain_db
            ));
        }
        for (i, b) in self.buses.iter().enumerate().take(3) {
            text.push_str(&format!(
                "\t<LabelBus{}>{}</LabelBus{}>\n",
                i + 1,
                xml_escape(&b.name),
                i + 1
            ));
        }
        for (i, b) in self.buses.iter().enumerate().skip(3) {
            text.push_str(&format!(
                "\t<LabelVirtualBus{}>{}</LabelVirtualBus{}>\n",
                i - 2,
                xml_escape(&b.name),
                i - 2
            ));
        }
        // Rebuild extension: the original's matrix EQ is stored separately
        // per channel (FUN_004f1a90). This console has one 6-band bus EQ, so it
        // goes in an element the original's loader ignores.
        for (i, b) in self.buses.iter().enumerate() {
            text.push_str(&format!(
                "\t<BusEQ index='{}' on='{}' g0='{:.2}' g1='{:.2}' g2='{:.2}' g3='{:.2}' g4='{:.2}' g5='{:.2}' />\n",
                i + 1,
                b.eq_on as u8,
                b.eq[0].gain,
                b.eq[1].gain,
                b.eq[2].gain,
                b.eq[3].gain,
                b.eq[4].gain,
                b.eq[5].gain
            ));
        }
        if let Some([w, h]) = self.window_size {
            let (x, y) = self
                .window_pos
                .map(|[x, y]| (x, y))
                .unwrap_or((40.0, 40.0));
            text.push_str(&format!(
                "\t<Window x='{:.1}' y='{:.1}' width='{:.1}' height='{:.1}' maximized='{}' zoom='{:.2}' />\n",
                x.clamp(-32000.0, 32000.0),
                y.clamp(-32000.0, 32000.0),
                w.clamp(800.0, 5000.0),
                h.clamp(500.0, 4000.0),
                if self.window_maximized { 1 } else { 0 },
                self.ui_zoom.clamp(0.5, 3.0)
            ));
        }
        text.push_str("</AudioMixerProParameters>\n");
        text.push_str("<AudioMixerProRecorder>\n");
        // The tape deck's end-of-file action is the recorder Loop flag
        // (0x02000000, FUN_0052cd90 / FUN_0054bde0).
        text.push_str(&format!(
            "\t<RecordMode RecBus='0' DontPlay='0' Loop='{}' MultiTrack='0' TimerStop='{}' />\n",
            self.tape.loop_playback as u8, self.tape.timer_stop
        ));
        // FUN_00529c80 writes only the first five input and first five bus
        // arm bits even though FUN_0052cd90 reads eight of each.
        let arm = self.tape.arm;
        text.push_str(&format!(
            "\t<RecordArm ArmIn1='{}' ArmIn2='{}' ArmIn3='{}' ArmIn4='{}' ArmIn5='{}' ArmBus1='{}' ArmBus2='{}' ArmBus3='{}' ArmBus4='{}' ArmBus5='{}' />\n",
            arm & 1,
            (arm >> 1) & 1,
            (arm >> 2) & 1,
            (arm >> 3) & 1,
            (arm >> 4) & 1,
            (arm >> 16) & 1,
            (arm >> 17) & 1,
            (arm >> 18) & 1,
            (arm >> 19) & 1,
            (arm >> 20) & 1
        ));
        text.push_str(&format!(
            "\t<RecordParam FileType='{}' BitRes='{}' TargetSR='{}' nbChannel='{}' kbps='{}' preRecTime='{}' />\n",
            self.tape.rec_file_type,
            self.tape.rec_bit_depth,
            self.tape.rec_target_sr,
            self.tape.rec_channels,
            self.tape.rec_kbps,
            self.tape.rec_pre_rec
        ));
        text.push_str("</AudioMixerProRecorder>\n");
        // Rebuild extension for state the original keeps in the registry
        // (audio format, menu switches) and for the tape file itself.
        let latency_profile = match self.main_buffer {
            256 => "low",
            512 => "balanced",
            1024 => "safe",
            _ => "custom",
        };
        text.push_str(&format!(
            "<AppOptions sampleRate='{}' buffer='{}' mainBuffer='{}' latencyProfile='{}' engine='{}' limitRemote='{}' lock='{}' autoRestart='{}' autoRestartAll='{}' helpCaption='{}' loadOnStartup='{}' saveOnExit='{}' presetOnStartup='{}' />\n",
            self.sample_rate,
            self.buffer,
            self.main_buffer,
            latency_profile,
            self.engine_mode,
            self.limit_remote as u8,
            self.lock_gui as u8,
            self.auto_restart_a1 as u8,
            self.auto_restart_all as u8,
            self.help_caption as u8,
            self.load_on_startup as u8,
            self.save_on_exit as u8,
            self.show_preset_on_startup as u8
        ));
        let vc = self.vc.config();
        text.push_str(&format!(
            "\t<VirtualCables enabled='{}' cables='{}' bufferMs='{}' maxRate='{}' uacMode='{}' />\n",
            vc.enabled as u8,
            vc.cables,
            vc.buffer_ms,
            vc.max_rate,
            vc.uac_mode.as_str()
        ));
        text.push_str(&format!(
            "\t<VBAN enabled='{}' localPort='{}' quality='{}' />\n",
            self.vban_enabled as u8, self.vban_port, self.vban_quality
        ));
        text.push_str(&format!(
            "\t<Ducking enabled='{}' source='{}' targets='{}' depthDb='{:.2}' thresholdDb='{:.2}' attackMs='{:.1}' releaseMs='{:.0}' />\n",
            self.duck.enabled as u8,
            self.duck.source.map(|s| s as i32).unwrap_or(-1),
            self.duck.targets,
            self.duck.depth_db,
            self.duck.threshold_db,
            self.duck.attack_ms,
            self.duck.release_ms
        ));
        for i in 0..self.active_strips {
            if let Some(StripInput::Vban(input)) = self.captures[i].as_ref() {
                if let Some((name, ip, port)) = Self::parse_vban_device(&input.device) {
                    text.push_str(&format!(
                        "\t<VBANStreamIn index='{}' in='{}' name=\"{}\" ip='{}' port='{}' NQ='{}' />\n",
                        i + 1,
                        i + 1,
                        xml_escape(&name),
                        xml_escape(&ip),
                        port,
                        self.vban_quality
                    ));
                }
            }
        }
        for (index, out) in self.vban_out.iter().enumerate() {
            let (ip, port) = out
                .target
                .rsplit_once(':')
                .unwrap_or((out.target.as_str(), "6980"));
            text.push_str(&format!(
                "\t<VBANStreamOut index='{}' bus='{}' strip='{}' name=\"{}\" ip='{}' port='{}' SR='{}' ch='{}' NQ='{}' format='{}' />\n",
                index + 1,
                out.bus,
                out.strip.map(|strip| strip as i64).unwrap_or(-1),
                xml_escape(&out.name),
                xml_escape(ip),
                port,
                out.sample_rate,
                out.channels,
                out.quality,
                out.format.code()
            ));
        }
        let route: String = self
            .tape
            .route
            .iter()
            .map(|r| if *r { '1' } else { '0' })
            .collect();
        text.push_str(&format!(
            "<TapeFile demo='{}' gain='{:.2}' route='{}'>{}</TapeFile>\n",
            self.tape.demo.map(|d| d.index() as i32).unwrap_or(-1),
            self.tape.gain_db,
            route,
            xml_escape(&self.tape.path)
        ));
        for path in self.tape_recent.iter().take(6) {
            text.push_str(&format!(
                "<TapeRecent>{}</TapeRecent>\n",
                xml_escape(&path.display().to_string())
            ));
        }
        // Rebuild extension: one VST3 insert per input strip. Unknown elements
        // are ignored by the original's tag-driven reader.
        for (slot, vst) in self.vsts.iter().take(self.active_strips).enumerate() {
            // Write every active slot, even empty ones, so loading a scene
            // clears a slot the scene does not use instead of leaving the old
            // plug-in behind.
            text.push_str(&format!(
                "<VstPlugin slot=\"{}\" path=\"{}\" bypass=\"{}\" />\n",
                slot + 1,
                xml_escape(&vst.path),
                vst.bypass as u8
            ));
        }
        text.push_str(&format!(
            "<Streamer enabled='{}' speed='{:.1}' />\n",
            if self.streamer_mode { 1 } else { 0 },
            self.streamer_speed.clamp(1.0, 30.0)
        ));
        text.push_str("</AudioMixerProSettings>\n");
        text
    }

    fn save_settings(&mut self) {
        let text = self.scene_document();
        let path = settings_file();
        // First run on a clean machine: the folder does not exist yet, and a
        // failed write here would mean the scene is never persisted at all.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        match std::fs::write(&path, text) {
            Ok(_) => {
                for slot in 0..self.vsts.len() {
                    if self.vsts[slot].path.is_empty() {
                        let _ = std::fs::remove_file(vst_state_file(slot));
                    } else {
                        let _ = self.vsts[slot].chain.save_state_to(&vst_state_file(slot));
                    }
                }
                self.toast(format!("Settings saved to {}", path.display()))
            }
            Err(e) => self.toast(format!("Could not save settings: {e}")),
        }
    }

    /// FUN_0052d990: name and comment are stored beside the timestamp around
    /// the same AudioMixerProParameters block that the normal scene uses.
    fn save_preset(&mut self) -> Result<PathBuf, String> {
        let doc = self.scene_document();
        let mut block = match (
            doc.find("<AudioMixerProParameters>"),
            doc.find("</AudioMixerProRecorder>"),
        ) {
            (Some(start), Some(end)) => doc[start..end + "</AudioMixerProRecorder>".len()].to_string(),
            _ => doc.clone(),
        };
        // The preset carries each strip's plug-in choice too, so recalling a
        // scene also recalls which plug-in runs on which channel.
        for (slot, vst) in self.vsts.iter().take(self.active_strips).enumerate() {
            block.push_str(&format!(
                "\n<VstPlugin slot=\"{}\" path=\"{}\" bypass=\"{}\" />",
                slot + 1,
                xml_escape(&vst.path),
                vst.bypass as u8
            ));
        }
        let name = {
            let trimmed = self.preset_name.trim();
            if trimmed.is_empty() {
                "Preset".to_string()
            } else {
                trimmed.to_string()
            }
        };
        let stamp = preset_stamp_now();
        let preset = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<AudioMixerProPresetScene>\n\
             <PresetName index='0' fdevice='0' >{}</PresetName>\n\
             <PresetComment>{}</PresetComment>\n\
             <PresetTimeStamp>{}</PresetTimeStamp>\n{}\n\
             </AudioMixerProPresetScene>\n",
            xml_escape(&name),
            xml_escape(self.preset_comment.trim()),
            xml_escape(&stamp),
            block
        );
        let dir = presets_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create preset folder: {e}"))?;
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let mut path = dir.join(format!("{}.xml", safe.trim()));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{}.xml", format!("{} ({})", safe.trim(), n)));
            n += 1;
        }
        std::fs::write(&path, preset).map_err(|e| format!("Could not write preset: {e}"))?;
        self.preset_active = name;
        self.preset_entries = scan_presets();
        Ok(path)
    }

    fn load_preset(&mut self, path: &Path) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                self.load_settings_xml(&text);
                self.apply_saved_inputs();
                self.apply_vban_config();
                self.restore_vst(false);
                self.preset_active = xml_elements(&text, "PresetName")
                    .first()
                    .map(|(_, body)| xml_unescape(body))
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("Preset")
                            .to_string()
                    });
                self.toast(format!("Preset scene \"{}\" loaded", self.preset_active));
            }
            Err(e) => self.preset_status = format!("Could not read preset: {e}"),
        }
    }

    fn open_presets(&mut self) {
        self.preset_entries = scan_presets();
        self.preset_status.clear();
        self.show_presets = true;
    }

    /// Read only the two persistence switches from the scene. Doing this
    /// before the load decision keeps "Load Settings on Startup" off on the
    /// next run as well, without applying any strip values.
    fn read_persistence_flags(&mut self) {
        let path = settings_file();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Some((attrs, _)) = xml_elements(&text, "AppOptions").first() {
            if let Some(v) = bool_attr(attrs, "loadOnStartup") {
                self.load_on_startup = v;
            }
            if let Some(v) = bool_attr(attrs, "saveOnExit") {
                self.save_on_exit = v;
            }
            if let Some(v) = bool_attr(attrs, "presetOnStartup") {
                self.show_preset_on_startup = v;
            }
        }
    }

    fn load_settings(&mut self, announce: bool) {
        self.load_settings_parse(announce);
        // <InputDev> only stores a name; open the stream again so the restored
        // HW INPUT 1-3 really work after a restart.
        self.apply_saved_inputs();
        self.apply_vban_config();
    }

    /// Read the scene without opening any audio device. Construction uses this
    /// so the window is painted before captures open; the Load button still
    /// uses `load_settings`, which opens them immediately.
    fn load_settings_parse(&mut self, announce: bool) {
        let path = settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.load_settings_xml(&text);
                if announce {
                    self.toast("Settings loaded");
                }
            }
            Err(_) => {
                // Settings written by the earlier build still load.
                self.load_settings_legacy();
            }
        }
    }

    /// One start-up frame. Engine, cables and device opens each get their own
    /// frame, after the console has painted, so a blocked WASAPI call behind
    /// the Windows audio service can never hide the window.
    fn startup_step_frame(&mut self, dt: f64) {
        match self.startup_step {
            StartupStep::Done => {}
            StartupStep::Engine => {
                if self.shot_path.is_some() {
                    // Screenshot runs have no human to wait for, so the open is
                    // still synchronous here - but bounded, so a wedged device
                    // cannot trap the capture run forever.
                    diag_log("startup: opening audio engine (screenshot)");
                    self.audio.start_stream_async();
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(3);
                    loop {
                        if self.audio.poll_start().is_some() {
                            break;
                        }
                        if std::time::Instant::now() >= deadline {
                            self.audio.stop_stream();
                            self.audio.available = false;
                            self.audio.status = "Audio device did not answer".to_string();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    self.sample_rate = self.audio.sample_rate.max(8_000);
                    self.audio.set_strip_count(self.active_strips);
                    self.startup_step = StartupStep::Cables;
                    self.startup_wait = 0.0;
                    self.startup_settle = 0.0;
                    if !self.audio.available {
                        diag_log(format!(
                            "startup: audio engine unavailable: {}",
                            self.audio.status
                        ));
                    }
                    return;
                }
                if self.audio.pending_start.is_none() && !self.audio.available && self.startup_wait == 0.0 {
                    diag_log("startup: opening audio engine");
                    self.audio.start_stream_async();
                }
                self.startup_wait += dt;
                let outcome = self.audio.poll_start();
                match outcome {
                    Some(ok) => {
                        self.sample_rate = self.audio.sample_rate.max(8_000);
                        self.audio.set_strip_count(self.active_strips);
                        if ok {
                            diag_log("startup: audio engine ready");
                        } else {
                            diag_log(format!(
                                "startup: audio engine unavailable: {}",
                                self.audio.status
                            ));
                            self.toast_for(format!("Audio engine: {}", self.audio.status), 8.0);
                        }
                        self.startup_wait = 0.0;
                        self.startup_settle = 0.0;
                        self.startup_step = StartupStep::Cables;
                    }
                    None if self.startup_wait < 8.0 => {}
                    None => {
                        // The device open is wedged inside the audio service.
                        // Abandon it (the helper thread stays blocked, the UI
                        // does not) and continue without audio.
                        self.audio.stop_stream();
                        self.audio.available = false;
                        self.audio.status = "Audio device did not answer".to_string();
                        diag_log("startup: audio engine timed out");
                        self.toast_for(
                            "Audio device did not answer - running without audio",
                            10.0,
                        );
                        self.startup_wait = 0.0;
                        self.startup_settle = 0.0;
                        self.startup_step = StartupStep::Cables;
                    }
                }
            }
            StartupStep::Cables => {
                // Only the cached list here; the probe thread does the
                // enumeration so a wedged endpoint cannot freeze the console.
                if self.last_outputs.is_empty() || self.last_inputs.is_empty() {
                    self.probe_devices_now();
                    self.probe_inputs_now();
                    self.startup_wait += dt;
                    if self.startup_wait < 3.0 {
                        return;
                    }
                    diag_log("startup: device probe still pending, continuing");
                }
                self.driver_mode = driver_cables_present_from(&self.last_outputs);
                diag_log(format!("startup: driver cables present = {}", self.driver_mode));
                if self.driver_mode {
                    diag_log("startup: kernel virtual cable driver detected");
                }
                if self.vc_cfg.enabled {
                    diag_log("startup: starting USB virtual cables");
                    self.vc.apply(self.vc_cfg.clone());
                    diag_log("startup: USB virtual cables applied");
                }
                self.startup_step = StartupStep::Devices;
                self.startup_wait = 0.0;
            }
            StartupStep::Devices => {
                let attached = !self.vc_cfg.enabled || self.vc.status().attached;
                self.startup_wait += dt;
                if attached {
                    // "attached" only means the attach command returned; the
                    // endpoints still need a moment to enumerate. Opening a
                    // capture inside that window is what blocks in the audio
                    // service.
                    self.startup_settle += dt;
                    if self.startup_settle < 0.6 {
                        return;
                    }
                } else if self.startup_wait < 8.0 {
                    return;
                }
                diag_log(if attached {
                    "startup: cables attached, opening devices"
                } else {
                    "startup: cable attach still pending, opening available devices"
                });
                // An empty probe means the audio service never answered
                // within the start-up window. Every capture call (listing or
                // opening) goes through that same service and would block the
                // UI thread for minutes, so leave captures closed and let the
                // watchdog try again once the service recovers.
                if self.audio.available && (self.last_outputs.is_empty() || self.last_inputs.is_empty()) {
                    diag_log("startup: audio service not answering, capture devices left closed");
                    self.toast_for(
                        "Audio service is not responding - input devices left off",
                        10.0,
                    );
                } else if self.audio.available {
                    self.open_default_input();
                    self.apply_saved_inputs();
                }
                if self.audio.available {
                    // Plug-ins do not touch the audio service, so they are
                    // restored even when the device probe timed out.
                    let _ = self.apply_audio_format();
                    self.restore_vst(false);
                } else {
                    diag_log("startup: no engine, capture devices left closed");
                }
                self.disable_routes_without_input();
                if let Some(index) = self.startup_demo.take() {
                    if let Some(kind) = DemoKind::from_index(index) {
                        self.select_demo(kind);
                        self.on_transport(Transport::Play);
                    }
                }
                if std::env::args().any(|arg| arg == "--bus-demo") {
                    self.tape.route = [false, false, false, true, true, false];
                    self.select_demo(DemoKind::Track1);
                    self.on_transport(Transport::Play);
                }
                self.startup_step = StartupStep::Done;
                diag_log("startup: done");
            }
        }
    }

    fn advance_startup(&mut self, dt: f64) {
        if self.startup_step != StartupStep::Done {
            self.startup_step_frame(dt);
        }
    }

    /// Screenshot runs have no human waiting, so they run every step at once.
    fn run_startup_blocking(&mut self) {
        for _ in 0..8 {
            if self.startup_step == StartupStep::Done {
                break;
            }
            self.startup_step_frame(600.0);
        }
    }

    /// A lit A1 button over a strip with no capture device is misleading, so
    /// those strips keep A1 off. Runs once the scene's devices are open, so a
    /// strip that really has an input keeps its saved routing.
    fn disable_routes_without_input(&mut self) {
        for i in 0..3 {
            // Judge by the chosen device, not by whether its capture happened
            // to be open at this instant: a cable endpoint can enumerate a
            // second after the mix starts, and clearing its routing then would
            // silence a strip that is about to work.
            let name = self.strips[i].device.trim();
            let has_device = !name.is_empty()
                && !name.eq_ignore_ascii_case("No input")
                && !name.eq_ignore_ascii_case("No input device found")
                && !name.eq_ignore_ascii_case("No device");
            if !has_device {
                self.strips[i].route = [false; audio::MAX_BUSES];
            }
        }
    }

    /// Open the default microphone for HW INPUT 1 when the scene did not name a
    /// device for that strip. If it did, `apply_saved_inputs` opens that one.
    fn open_default_input(&mut self) {
        if self.captures[0].is_some() {
            return;
        }
        let adopt_name = self.strips[0].device.is_empty()
            || self.strips[0].device.eq_ignore_ascii_case("WDM: Microphone");
        if !adopt_name {
            return;
        }
        let capture = InputCapture::open(None);
        if capture.available {
            self.strips[0].device = capture.device.clone();
            self.audio.attach_input(0, capture.shared());
            self.captures[0] = Some(StripInput::Cpal(capture));
        }
    }

    fn load_settings_xml(&mut self, text: &str) {
        self.pending_auto_assign.clear();
        let mut tape_path = String::new();
        let mut tape_demo: i32 = -1;
        let mut recent: Vec<(usize, PathBuf)> = Vec::new();
        let mut recent_order = 0usize;

        // A scene may add virtual inputs past the original five. Discover the
        // highest strip referenced before applying labels/devices/VST slots.
        let mut highest = BASE_STRIPS;
        for tag in ["Strip", "InputDev"] {
            for (attrs, _) in xml_elements(text, tag) {
                if let Some(idx) = xml_attr(attrs, "index")
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    highest = highest.max(idx.min(MAX_STRIPS));
                }
            }
        }
        for (attrs, _) in xml_elements(text, "VBANStreamIn") {
            if let Some(idx) = xml_attr(attrs, "in")
                .or_else(|| xml_attr(attrs, "index"))
                .and_then(|value| value.parse::<usize>().ok())
            {
                highest = highest.max(idx.min(MAX_STRIPS));
            }
        }
        for (attrs, _) in xml_elements(text, "VstPlugin") {
            if let Some(slot) = xml_attr(attrs, "slot")
                .and_then(|value| value.parse::<usize>().ok())
            {
                highest = highest.max(slot.min(MAX_STRIPS));
            }
        }
        self.active_strips = highest.clamp(BASE_STRIPS, MAX_STRIPS);
        self.audio.set_strip_count(self.active_strips);

        // One element per strip. A scene from the earlier single-slot build has
        // no slot attribute, so the first element still lands on strip 1.
        for (order, (attrs, _)) in xml_elements(text, "VstPlugin").iter().enumerate() {
            let slot = xml_attr(attrs, "slot")
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(1, MAX_STRIPS) - 1)
                .unwrap_or_else(|| order.min(MAX_STRIPS - 1));
            if let Some(value) = xml_attr(attrs, "path") {
                self.vsts[slot].path = xml_unescape(&value);
            }
            if let Some(value) = bool_attr(attrs, "bypass") {
                self.vsts[slot].bypass = value;
            }
        }

        if let Some((attrs, _)) = xml_elements(text, "Streamer").first() {
            self.streamer_mode = bool_attr(attrs, "enabled").unwrap_or(self.streamer_mode);
            if let Some(speed) = f32_attr(attrs, "speed") {
                self.streamer_speed = speed.clamp(1.0, 30.0);
            }
        }

        if let Some((attrs, _)) = xml_elements(text, "VirtualCables").first() {
            // Older scenes and CONFIG.ini had no bufferMs; use the balanced
            // default instead of inheriting the old 250 ms ring.
            self.vc_cfg.buffer_ms = 60;
            if let Some(value) = xml_attr(attrs, "bufferMs") {
                if let Ok(n) = value.parse::<u32>() {
                    self.vc_cfg.buffer_ms = n.clamp(20, 5000);
                }
            }
            if let Some(value) = xml_attr(attrs, "enabled") {
                self.vc_cfg.enabled = value == "1" || value.eq_ignore_ascii_case("true");
            }
            if let Some(value) = xml_attr(attrs, "cables") {
                if let Ok(n) = value.parse::<u8>() {
                    self.vc_cfg.cables = n.clamp(1, 32);
                }
            }
            if let Some(value) = xml_attr(attrs, "maxRate") {
                if let Ok(n) = value.parse::<u32>() {
                    self.vc_cfg.max_rate = n.clamp(44_100, 192_000);
                }
            }
            if let Some(value) = xml_attr(attrs, "uacMode") {
                if let Some(mode) = virtual_cables::Mode::parse(&value) {
                    self.vc_cfg.uac_mode = mode;
                }
            }
        }

        if let Some((attrs, _)) = xml_elements(text, "Ducking").first() {
            if let Some(value) = bool_attr(attrs, "enabled") {
                self.duck.enabled = value;
            }
            if let Some(value) = xml_attr(attrs, "source")
                .and_then(|v| v.trim().parse::<i32>().ok())
            {
                self.duck.source = if value >= 0 {
                    Some((value as usize).min(MAX_STRIPS - 1))
                } else {
                    None
                };
            }
            if let Some(value) = xml_attr(attrs, "targets")
                .and_then(|v| v.trim().parse::<u32>().ok())
            {
                self.duck.targets = value;
            }
            if let Some(value) = f32_attr(attrs, "depthDb") {
                self.duck.depth_db = value.clamp(-30.0, 0.0);
            }
            if let Some(value) = f32_attr(attrs, "thresholdDb") {
                self.duck.threshold_db = value.clamp(-60.0, -6.0);
            }
            if let Some(value) = f32_attr(attrs, "attackMs") {
                self.duck.attack_ms = value.clamp(1.0, 100.0);
            }
            if let Some(value) = f32_attr(attrs, "releaseMs") {
                self.duck.release_ms = value.clamp(20.0, 1000.0);
            }
        }

        if let Some((attrs, _)) = xml_elements(text, "VBAN").first() {
            if let Some(value) = xml_attr(attrs, "enabled") {
                self.vban_enabled = value == "1" || value.eq_ignore_ascii_case("true");
            }
            if let Some(value) = xml_attr(attrs, "localPort").and_then(|v| v.parse::<u16>().ok()) {
                self.vban_port = value;
            }
            if let Some(value) = xml_attr(attrs, "quality").and_then(|v| v.parse::<u8>().ok()) {
                self.vban_quality = value.min(4);
            }
        }
        self.vban_out.clear();
        for (attrs, _) in xml_elements(text, "VBANStreamOut") {
            let mut out = vban::VbanOutConfig::default();
            if let Some(value) = xml_attr(attrs, "bus").and_then(|v| v.parse::<usize>().ok()) {
                out.bus = value.min(audio::MAX_BUSES - 1);
            }
            if let Some(value) = xml_attr(attrs, "strip").and_then(|v| v.parse::<i64>().ok()) {
                if value >= 0 {
                    out.strip = Some((value as usize).min(MAX_STRIPS - 1));
                }
            }
            if let Some(value) = xml_attr(attrs, "name") {
                out.name = xml_unescape(&value);
            }
            let ip = xml_attr(attrs, "ip").map(|v| xml_unescape(&v)).unwrap_or_default();
            let port = xml_attr(attrs, "port").unwrap_or_else(|| "6980".to_string());
            out.target = if ip.is_empty() {
                format!("127.0.0.1:{port}")
            } else if ip.contains(':') && !ip.starts_with('[') {
                format!("[{ip}]:{port}")
            } else {
                format!("{ip}:{port}")
            };
            if let Some(value) = xml_attr(attrs, "SR").and_then(|v| v.parse::<u32>().ok()) {
                out.sample_rate = value;
            }
            if let Some(value) = xml_attr(attrs, "ch").and_then(|v| v.parse::<usize>().ok()) {
                out.channels = value.clamp(1, 2);
            }
            if let Some(value) = xml_attr(attrs, "NQ").and_then(|v| v.parse::<u8>().ok()) {
                out.quality = value.min(4);
            }
            if let Some(value) = xml_attr(attrs, "format").and_then(|v| v.parse::<u8>().ok()) {
                out.format = vban::VbanFormat::from_code(value).unwrap_or(vban::VbanFormat::Float32);
            }
            self.vban_out.push(out);
        }

        for (i, s) in self
            .strips
            .iter_mut()
            .take(self.active_strips)
            .enumerate()
        {
            let label = if i < 3 {
                xml_elements(text, &format!("LabelStrip{}", i + 1))
                    .first()
                    .map(|(_, body)| xml_unescape(body))
            } else {
                xml_elements(text, &format!("LabelVirtualStrip{}", i - 2))
                    .first()
                    .map(|(_, body)| xml_unescape(body))
            };
            if let Some(name) = label {
                if !name.is_empty() {
                    s.name = name;
                }
            }
        }
        for (i, b) in self.buses.iter_mut().enumerate() {
            let label = if i < 3 {
                xml_elements(text, &format!("LabelBus{}", i + 1))
                    .first()
                    .map(|(_, body)| xml_unescape(body))
            } else {
                xml_elements(text, &format!("LabelVirtualBus{}", i - 2))
                    .first()
                    .map(|(_, body)| xml_unescape(body))
            };
            if let Some(name) = label {
                if !name.is_empty() {
                    b.name = name;
                }
            }
        }

        // Window size written by this rebuild; the original ignores it.
        if let Some((attrs, _)) = xml_elements(text, "Window").first() {
            let width = xml_attr(attrs, "width").and_then(|v| v.parse::<f32>().ok());
            let height = xml_attr(attrs, "height").and_then(|v| v.parse::<f32>().ok());
            if let (Some(width), Some(height)) = (width, height) {
                if (400.0..=8000.0).contains(&width) && (300.0..=8000.0).contains(&height) {
                    self.window_size = Some([width, height]);
                }
            }
        }

        // FUN_0052b490: the scene block's Strip elements.
        for (attrs, _body) in xml_elements(text, "Strip") {
            let Some(idx) = xml_attr(attrs, "index").and_then(|v| v.parse::<usize>().ok()) else {
                continue;
            };
            if !(1..=MAX_STRIPS).contains(&idx) {
                continue;
            }
            let i = idx - 1;
            let s = &mut self.strips[i];
            if let Some(v) = bool_attr(attrs, "mute") {
                s.mute = v;
            }
            if let Some(v) = bool_attr(attrs, "solo") {
                s.solo = v;
            }
            if let Some(v) = bool_attr(attrs, "mono") {
                s.mono = v;
            }
            // FUN_0052b490: muc is forced off for strip index 4 (the fifth
            // strip, param_1 == 4).
            if let Some(v) = bool_attr(attrs, "muc") {
                s.mc = v && i != 4;
            }
            // FUN_0052b490 restores the engine's program-active bits from
            // prg_c (0x10) and prg_g (0x20).
            if let Some(v) = bool_attr(attrs, "prg_c") {
                s.comp_prg = v;
            }
            if let Some(v) = bool_attr(attrs, "prg_g") {
                s.gate_prg = v;
            }
            for (n, name) in ["busa", "busa2", "busa3", "busb", "busb2", "busb3"]
                .iter()
                .enumerate()
            {
                if let Some(v) = bool_attr(attrs, name) {
                    s.route[n] = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "dblevel").and_then(|v| v.parse::<f32>().ok()) {
                // FUN_0052b490 runs the strip level through atof only.
                s.gain_db = v;
            }
            if let Some(v) = f32_attr(attrs, "dblimit") {
                s.limit = v.clamp(LIMIT_MIN, GAIN_MAX);
            }
            if let Some(v) = xml_attr(attrs, "karaoke").and_then(|v| v.parse::<i32>().ok()) {
                s.karaoke = v.clamp(0, 4) as u8;
            }
            if let Some(v) = bool_attr(attrs, "vaio") {
                s.kind = if v { StripKind::Virtual } else { StripKind::Hardware };
            }
            if let Some(v) = f32_attr(attrs, "audibility_c") {
                s.comp = v.clamp(AUDIBILITY_MIN, AUDIBILITY_MAX);
            }
            if let Some(v) = f32_attr(attrs, "audibility_g") {
                s.gate = v.clamp(AUDIBILITY_MIN, AUDIBILITY_MAX);
            }
            // FUN_0052b490 paneltype: clear both bits (mask 0xff0fffff), then
            // 1 -> 0x100000 Color, 2 -> 0x200000 Mod/Fx; anything else Pan.
            if let Some(v) = xml_attr(attrs, "paneltype").and_then(|v| v.parse::<i32>().ok()) {
                s.panel_type = match v {
                    1 => 1,
                    2 => 2,
                    _ => 0,
                };
            }
            if let Some(v) = f32_attr(attrs, "ColorPanelx") {
                s.color_panel[0] = v.clamp(PANEL_X_MIN, PANEL_X_MAX);
            }
            if let Some(v) = f32_attr(attrs, "ColorPanely") {
                s.color_panel[1] = v.clamp(PANEL3D_Y_MIN, PANEL3D_Y_MAX);
            }
            if let Some(v) = f32_attr(attrs, "ModPanelx") {
                s.mod_panel[0] = v.clamp(PANEL_X_MIN, PANEL_X_MAX);
            }
            if let Some(v) = f32_attr(attrs, "ModPanely") {
                s.mod_panel[1] = v.clamp(PANEL3D_Y_MIN, PANEL3D_Y_MAX);
            }
            if let Some(v) = f32_attr(attrs, "Panel3Dx") {
                s.intelli[0] = v.clamp(PANEL_X_MIN, PANEL_X_MAX);
                s.pan = s.intelli[0];
            }
            if let Some(v) = f32_attr(attrs, "Panel3Dy") {
                // FUN_0052b490: hardware strips clamp panel y to 0..1, the
                // virtual strips clamp it to -0.5..+0.5.
                s.intelli[1] = if i < 3 {
                    v.clamp(PANEL3D_Y_MIN, PANEL3D_Y_MAX)
                } else {
                    v.clamp(PANEL_Y_MIN, PANEL_Y_MAX)
                };
            }
            if let Some(v) = f32_attr(attrs, "EQGain1") {
                s.eq[0] = v.clamp(EQ_GAIN_MIN, EQ_GAIN_MAX);
            }
            if let Some(v) = f32_attr(attrs, "EQGain2") {
                s.eq[1] = v.clamp(EQ_GAIN_MIN, EQ_GAIN_MAX);
            }
            if let Some(v) = f32_attr(attrs, "EQGain3") {
                s.eq[2] = v.clamp(EQ_GAIN_MIN, EQ_GAIN_MAX);
            }
            // The strip's built-in pan, compressor, gate and 3-band EQ are no
            // longer on the panel: every channel runs a VST3 insert instead.
            // Their saved values are deliberately dropped, otherwise a scene
            // would keep processors running that cannot be seen or adjusted.
            s.pan = 0.0;
            s.comp = 0.0;
            s.gate = 0.0;
            s.eq = [0.0, 0.0, 0.0];
            s.intelli = [0.0, 0.0];
            s.panel_type = 0;
            s.color_panel = [0.0, 0.0];
            s.mod_panel = [0.0, 0.0];
        }

        // FUN_0052c030: bus elements. BusMode is stored as a nibble.
        for (attrs, _body) in xml_elements(text, "Bus") {
            if xml_attr(attrs, "cell").is_some() || xml_attr(attrs, "channel").is_some() {
                continue; // matrix EQ elements, not a bus header
            }
            let Some(idx) = xml_attr(attrs, "index").and_then(|v| v.parse::<usize>().ok()) else {
                continue;
            };
            if !(1..=audio::MAX_BUSES).contains(&idx) {
                continue;
            }
            let i = idx - 1;
            let b = &mut self.buses[i];
            if let Some(v) = bool_attr(attrs, "mute") {
                b.mute = v;
            }
            if let Some(v) = bool_attr(attrs, "mono") {
                b.mono = if v { 1 } else { 0 };
            }
            if let Some(v) = bool_attr(attrs, "cross") {
                if v {
                    b.mono = 2;
                }
            }
            if let Some(v) = xml_attr(attrs, "BusMode").and_then(|v| v.parse::<i32>().ok()) {
                b.mode = (v & 0xf) as usize;
            }
            if let Some(v) = bool_attr(attrs, "EQon") {
                b.eq_on = v;
            }
            if let Some(v) = bool_attr(attrs, "SEL") {
                b.sel = v;
            }
            if let Some(v) = bool_attr(attrs, "monitor") {
                b.monitor = v;
            }
            if let Some(v) = xml_attr(attrs, "dblevel").and_then(|v| v.parse::<f32>().ok()) {
                // FUN_0052c030 runs the bus level through atof only.
                b.gain_db = v;
            }
        }

        for (attrs, _body) in xml_elements(text, "BusEQ") {
            let Some(idx) = xml_attr(attrs, "index").and_then(|v| v.parse::<usize>().ok()) else {
                continue;
            };
            if !(1..=audio::MAX_BUSES).contains(&idx) {
                continue;
            }
            let b = &mut self.buses[idx - 1];
            if let Some(v) = bool_attr(attrs, "on") {
                b.eq_on = v;
            }
            for (n, name) in ["g0", "g1", "g2", "g3", "g4", "g5"].iter().enumerate() {
                if let Some(v) = f32_attr(attrs, name) {
                    b.eq[n].gain = v.clamp(EQ_GAIN_MIN, EQ_GAIN_MAX);
                }
            }
        }

        if let Some((attrs, _)) = xml_elements(text, "VMOptions").first() {
            if let Some(v) = bool_attr(attrs, "MonitorOnSEL") {
                self.monitor_on_sel = v;
            }
        }
        if let Some((attrs, _)) = xml_elements(text, "RecordMode").first() {
            if let Some(v) = bool_attr(attrs, "Loop") {
                self.tape.loop_playback = v;
            }
            // FUN_0052cd90: TimerStop is a number, floored at 0; it is not a
            // flag like RecBus/DontPlay/Loop/MultiTrack (bits 0/24/25/26).
            if let Some(v) = xml_attr(attrs, "TimerStop").and_then(|v| v.trim().parse::<i32>().ok()) {
                self.tape.timer_stop = v.max(0);
            }
        }
        if let Some((attrs, _)) = xml_elements(text, "RecordParam").first() {
            // FUN_0052cd90 accepts FileType 1/2/3/100, BitRes>>3 in 1..4,
            // TargetSR 22050/24000/32000/44100/48000/88200/96000/176400/
            // 192000, nbChannel 2/4/6/8 (anything else becomes 2), one of the
            // 14 kbps values, and preRecTime 0..20 s.
            if let Some(v) = xml_attr(attrs, "FileType").and_then(|v| v.trim().parse::<i32>().ok()) {
                if REC_FILE_TYPES.contains(&v) {
                    self.tape.rec_file_type = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "BitRes").and_then(|v| v.trim().parse::<u32>().ok()) {
                let index = v >> 3;
                if (1..=4).contains(&index) {
                    self.tape.rec_bit_depth = index * 8;
                }
            }
            if let Some(v) = xml_attr(attrs, "TargetSR").and_then(|v| v.trim().parse::<u32>().ok()) {
                if REC_TARGET_RATES.contains(&v) {
                    self.tape.rec_target_sr = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "nbChannel").and_then(|v| v.trim().parse::<u32>().ok()) {
                self.tape.rec_channels = if REC_CHANNELS.contains(&v) { v } else { 2 };
            }
            if let Some(v) = xml_attr(attrs, "kbps").and_then(|v| v.trim().parse::<u32>().ok()) {
                if REC_KBPS.contains(&v) {
                    self.tape.rec_kbps = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "preRecTime").and_then(|v| v.trim().parse::<i32>().ok()) {
                self.tape.rec_pre_rec = v.clamp(0, REC_PRE_REC_MAX_S);
            }
        }
        if let Some((attrs, _)) = xml_elements(text, "RecordArm").first() {
            // FUN_0052cd90: ArmIn1-8 are bits 0..7 and ArmBus1-8 bits 16..23.
            let mut arm = 0u32;
            for n in 1..=8 {
                if bool_attr(attrs, &format!("ArmIn{n}")).unwrap_or(false) {
                    arm |= 1 << (n - 1);
                }
                if bool_attr(attrs, &format!("ArmBus{n}")).unwrap_or(false) {
                    arm |= 1 << (15 + n);
                }
            }
            self.tape.arm = arm;
        }
        if let Some((attrs, _)) = xml_elements(text, "AppOptions").first() {
            let has_latency_profile = xml_attr(attrs, "latencyProfile").is_some();
            if let Some(v) = xml_attr(attrs, "sampleRate").and_then(|v| v.parse::<u32>().ok()) {
                if SAMPLE_RATES.contains(&v) {
                    self.sample_rate = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "buffer").and_then(|v| v.parse::<u32>().ok()) {
                self.buffer = v;
            }
            if let Some(v) = xml_attr(attrs, "mainBuffer").and_then(|v| v.parse::<u32>().ok()) {
                self.main_buffer = v.min(MAIN_BUFFER_MAX);
                // 1024 was the old default; moving it to the balanced 512 is
                // a one-time latency improvement. A saved latencyProfile means
                // the person chose the value deliberately.
                if !has_latency_profile && self.main_buffer == 1024 {
                    self.main_buffer = 512;
                }
            }
            if let Some(v) = xml_attr(attrs, "engine").and_then(|v| v.parse::<usize>().ok()) {
                self.engine_mode = v.min(1);
            }
            if let Some(v) = bool_attr(attrs, "limitRemote") {
                self.limit_remote = v;
            }
            if let Some(v) = bool_attr(attrs, "lock") {
                self.lock_gui = v;
            }
            if let Some(v) = bool_attr(attrs, "autoRestart") {
                self.auto_restart_a1 = v;
            }
            if let Some(v) = bool_attr(attrs, "autoRestartAll") {
                self.auto_restart_all = v;
            }
            if let Some(v) = bool_attr(attrs, "helpCaption") {
                self.help_caption = v;
            }
            if let Some(v) = bool_attr(attrs, "loadOnStartup") {
                self.load_on_startup = v;
            }
            if let Some(v) = bool_attr(attrs, "saveOnExit") {
                self.save_on_exit = v;
            }
            if let Some(v) = bool_attr(attrs, "presetOnStartup") {
                self.show_preset_on_startup = v;
            }
        }
        // FUN_0052ab70: <OptionDev mme wdm ks asio srasio msA1 msA2 msA3>.
        // MME/WDM/KS accept 128..=2048, ASIO 0 or 64..=1024, and each latency
        // 0..=500 ms; values outside those bounds are ignored, not clamped.
        if let Some((attrs, _)) = xml_elements(text, "OptionDev").first() {
            for (name, slot) in [
                ("mme", &mut self.dev_mme),
                ("wdm", &mut self.dev_wdm),
                ("ks", &mut self.dev_ks),
            ] {
                if let Some(v) = xml_attr(attrs, name).and_then(|v| v.trim().parse::<u32>().ok()) {
                    if (DEV_BUFFER_MIN..=DEV_BUFFER_MAX).contains(&v) {
                        *slot = v;
                    }
                }
            }
            if let Some(v) = xml_attr(attrs, "asio").and_then(|v| v.trim().parse::<u32>().ok()) {
                if v == 0 || (64..=ASIO_BUFFER_MAX).contains(&v) {
                    self.dev_asio = v;
                }
            }
            if let Some(v) = xml_attr(attrs, "srasio") {
                self.dev_srasio = v.trim() != "0";
            }
            for (n, name) in ["msA1", "msA2", "msA3"].iter().enumerate() {
                if let Some(v) = f32_attr(attrs, name) {
                    if (0.0..=DEV_MS_MAX).contains(&v) {
                        self.bus_latency_ms[n] = v;
                    }
                }
            }
        }
        for (attrs, body) in xml_elements(text, "InputDev") {
            if let Some(idx) = xml_attr(attrs, "index").and_then(|v| v.parse::<usize>().ok()) {
                if (1..=MAX_STRIPS).contains(&idx) {
                    if let Some(name) = xml_attr(attrs, "name") {
                        let name = xml_unescape(&name);
                        if name == "-" || name.trim().is_empty() {
                            if idx <= 3 {
                                self.strips[idx - 1].device.clear();
                            } else {
                                self.strips[idx - 1].device = if idx == 4 {
                                    "Virtual 1 (VAIO)".to_string()
                                } else if idx == 5 {
                                    "Virtual 2 (AUX)".to_string()
                                } else {
                                    "No input".to_string()
                                };
                            }
                        } else {
                            self.strips[idx - 1].device = name;
                            if idx <= 3 {
                                if let Some(t) = xml_attr(attrs, "type").and_then(|v| v.trim().parse::<i32>().ok()) {
                                    self.input_types[idx - 1] = t;
                                }
                            }
                        }
                    }
                }
            }
            let _ = body;
        }
        // Incoming VBAN bindings are primarily restored from InputDev; this
        // fallback keeps a scene that only carries the original-style stream
        // entries working too.
        for (attrs, _) in xml_elements(text, "VBANStreamIn") {
            let raw = xml_attr(attrs, "in")
                .or_else(|| xml_attr(attrs, "index"))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let strip = raw.saturating_sub(1).min(MAX_STRIPS - 1);
            if strip >= self.active_strips || self.strips[strip].device.starts_with("VBAN: ") {
                continue;
            }
            let Some(name) = xml_attr(attrs, "name") else {
                continue;
            };
            let name = xml_unescape(&name);
            if name.trim().is_empty() {
                continue;
            }
            let ip = xml_attr(attrs, "ip")
                .map(|v| xml_unescape(&v))
                .unwrap_or_default();
            let port = xml_attr(attrs, "port").unwrap_or_else(|| "6980".to_string());
            self.strips[strip].device = format!("VBAN: {name} @ {ip}:{port}");
        }
        for (attrs, _body) in xml_elements(text, "OutputDev") {
            if let Some(idx) = xml_attr(attrs, "index").and_then(|v| v.parse::<usize>().ok()) {
                if (1..=audio::MAX_BUSES).contains(&idx) {
                    if let Some(name) = xml_attr(attrs, "name") {
                        let name = xml_unescape(&name);
                        if name == "-" {
                            self.buses[idx - 1].device.clear();
                            if idx <= 3 {
                                for n in 0..8 {
                                    if let Some(v) = xml_attr(attrs, &format!("pOut{}", n + 1))
                                        .and_then(|v| v.trim().parse::<i32>().ok())
                                    {
                                        self.output_patch[idx - 1][n] = v.clamp(0, OUT_PATCH_MAX);
                                    }
                                }
                            }
                        } else {
                            self.buses[idx - 1].device = name;
                            if let Some(t) = xml_attr(attrs, "type").and_then(|v| v.trim().parse::<i32>().ok()) {
                                self.output_types[idx - 1] = t;
                            }
                        }
                    }
                }
            }
        }
        for bus in 0..3 {
            let name = self.buses[bus].device.clone();
            let placeholder = name.starts_with("WDM:")
                || name.starts_with("ASIO:")
                || name == "Virtual Output 1"
                || name == "Virtual Output 2";
            if placeholder {
                self.buses[bus].device.clear();
            }
        }

        if let Some((attrs, body)) = xml_elements(text, "TapeFile").first() {
            tape_demo = xml_attr(attrs, "demo")
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(-1);
            if let Some(v) = f32_attr(attrs, "gain") {
                self.tape.gain_db = v;
            }
            if let Some(route) = xml_attr(attrs, "route") {
                for (n, c) in route.chars().take(audio::MAX_BUSES).enumerate() {
                    self.tape.route[n] = c == '1';
                }
            }
            tape_path = xml_unescape(body);
        }
        for (_attrs, body) in xml_elements(text, "TapeRecent") {
            let path = xml_unescape(body);
            if !path.is_empty() {
                recent.push((recent_order, PathBuf::from(path)));
                recent_order += 1;
            }
        }

        if tape_demo >= 0 {
            if let Some(kind) = DemoKind::from_index(tape_demo as usize) {
                self.select_demo(kind);
            }
        } else if !tape_path.is_empty() {
            if Path::new(&tape_path).exists() {
                self.load_wav_file(PathBuf::from(&tape_path));
            } else {
                self.tape.path = tape_path;
                self.tape_error = Some(format!(
                    "The saved file is no longer on disk: {}",
                    self.tape.path
                ));
            }
        }
        recent.sort_by_key(|(i, _)| *i);
        self.tape_recent = recent.into_iter().map(|(_, p)| p).take(6).collect();
    }

    /// 1.0 for the fader under the running light, fading for the two behind
    /// it, 0.0 when streamer mode is off. Strips come first, then buses.
    fn streamer_brightness(&self, index: usize) -> f32 {
        if !self.streamer_mode {
            return 0.0;
        }
        let total = self.active_strips + audio::MAX_BUSES;
        if total == 0 {
            return 0.0;
        }
        let head = ((self.time * self.streamer_speed as f64) as usize) % total;
        match (head + total - index % total) % total {
            0 => 1.0,
            1 => 0.45,
            2 => 0.18,
            _ => 0.0,
        }
    }

    /// The strip cards fill `start..start+5`. A bank with fewer strips than
    /// that shows empty slots
    /// virtual inputs each.
    fn bank_for_strip(index: usize) -> usize {
        if index < BASE_STRIPS {
            0
        } else {
            1 + (index - BASE_STRIPS) / 5
        }
    }

    fn bank_start(bank: usize) -> usize {
        if bank == 0 {
            0
        } else {
            BASE_STRIPS + (bank - 1) * 5
        }
    }

    /// Lowest unused `Microphone (Virtual Cable NN)` with NN >= 03. Cables 01
    /// and 02 carry B1/B2, so input strips start at cable 03.
    fn next_free_virtual_cable(&self) -> Option<String> {
        let inputs = self.last_inputs.clone();
        let used: Vec<String> = self
            .strips
            .iter()
            .take(self.active_strips)
            .map(|s| s.device.to_ascii_lowercase())
            .collect();
        Self::next_free_virtual_cable_from(&inputs, &used)
    }

    fn next_free_virtual_cable_from(inputs: &[String], used: &[String]) -> Option<String> {
        let mut best: Option<(u32, String)> = None;
        for name in inputs {
            let lower = name.to_ascii_lowercase();
            let Some(rest) = lower
                .strip_prefix("microphone array (virtual cable ")
                .or_else(|| lower.strip_prefix("microphone (virtual cable "))
                .or_else(|| lower.strip_prefix("microphone (usb cable "))
                .or_else(|| lower.strip_prefix("microphone array (usb cable "))
            else {
                continue;
            };
            let Some(number) = rest.strip_suffix(')') else {
                continue;
            };
            let Ok(number) = number.trim().parse::<u32>() else {
                continue;
            };
            // Every cable is bidirectional, so any free cable can feed a
            // virtual strip (cables already used by a strip are skipped).
            if used.iter().any(|d| d == &lower) {
                continue;
            }
            if best
                .as_ref()
                .map(|(lowest, _)| number < *lowest)
                .unwrap_or(true)
            {
                best = Some((number, name.clone()));
            }
        }
        best.map(|(_, name)| name)
    }

    fn add_virtual_input(&mut self) {
        if self.active_strips >= MAX_STRIPS {
            self.toast(format!("Maximum of {MAX_STRIPS} input strips reached"));
            return;
        }
        let idx = self.active_strips;
        let mut strip = Strip::new(idx);
        strip.kind = StripKind::Virtual;
        strip.name = format!("VIRTUAL {}", idx - 2);
        strip.device = "No input".to_string();
        strip.route = std::array::from_fn(|bus| bus == 0);
        strip.tape_feed = false;
        self.strips[idx] = strip;
        self.captures[idx] = None;
        let chain = self.audio.vst(idx);
        chain.close_editor();
        chain.unload();
        self.vsts[idx] = VstSlot::new(chain);
        self.active_strips += 1;
        self.audio.set_strip_count(self.active_strips);
        self.strip_bank = Self::bank_for_strip(idx);

        if let Some(cable) = self.next_free_virtual_cable() {
            let capture = InputCapture::open(Some(&cable));
            if capture.available {
                self.strips[idx].device = capture.device.clone();
                self.audio.attach_input(idx, capture.shared());
                self.captures[idx] = Some(StripInput::Cpal(capture));
                self.toast(format!(
                    "Virtual {} added - listening to {cable}",
                    idx - 2
                ));
                return;
            }
        }
        self.pending_auto_assign.push(idx);
        self.toast(format!("Virtual {} added - choose an input", idx - 2));
        self.picker = Some(Picker::Strip(idx));
    }

    fn retry_pending_auto_assign(&mut self) {
        if self.pending_auto_assign.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending_auto_assign);
        pending.retain(|&idx| {
            idx < self.active_strips && self.strips[idx].device == "No input"
        });
        for idx in pending {
            if let Some(cable) = self.next_free_virtual_cable() {
                let capture = InputCapture::open(Some(&cable));
                if capture.available {
                    self.strips[idx].device = capture.device.clone();
                    self.audio.attach_input(idx, capture.shared());
                    self.captures[idx] = Some(StripInput::Cpal(capture));
                    self.toast(format!(
                        "Virtual {} listening to {cable}",
                        idx - 2
                    ));
                    continue;
                }
            }
            self.pending_auto_assign.push(idx);
        }
    }

    /// Removal is last-in-first-out so active strip/VST slot indexes never
    /// move; the audio engine's preallocated slots stay aligned.
    fn remove_last_virtual_input(&mut self) {
        if self.active_strips <= BASE_STRIPS {
            return;
        }
        let idx = self.active_strips - 1;
        self.vban.unbind(idx);
        self.captures[idx] = None;
        self.audio.detach_input(idx);
        let chain = self.audio.vst(idx);
        if let Some(slot) = self.vsts.get_mut(idx) {
            slot.chain.close_editor();
            slot.chain.unload();
            *slot = VstSlot::new(chain);
        }
        if self.vst_panel == Some(idx) {
            self.vst_panel = None;
        }
        self.pending_auto_assign.retain(|&x| x != idx);
        self.strips[idx] = Strip::new(idx);
        self.active_strips -= 1;
        self.audio.set_strip_count(self.active_strips);
        if self.strip_bank > Self::bank_for_strip(self.active_strips - 1) {
            self.strip_bank = Self::bank_for_strip(self.active_strips - 1);
        }
        self.toast(format!("Virtual {} removed", idx - 2));
    }

    fn parse_vban_device(device: &str) -> Option<(String, String, u16)> {
        let rest = device.strip_prefix("VBAN: ")?;
        let (name, address) = rest.rsplit_once(" @ ")?;
        let (ip, port) = address.rsplit_once(':')?;
        Some((name.to_string(), ip.to_string(), port.parse().ok()?))
    }

    fn set_vban_input(&mut self, i: usize, name: &str, ip: &str, port: u16, quality: u8) {
        self.vban.unbind(i);
        let sink = self.vban.new_sink();
        self.vban.bind(i, name, ip, quality, sink.clone());
        let device = format!("VBAN: {name} @ {ip}:{port}");
        self.strips[i].device = device.clone();
        self.strips[i].tape_feed = false;
        self.audio.attach_input(i, sink.clone());
        self.captures[i] = Some(StripInput::Vban(VbanInput::new(sink, device)));
    }

    /// Re-open the inputs named in a restored scene. Only names that exist on
    /// this machine are opened; a placeholder or an unplugged device stays
    /// visible in the strip header so the person can reconnect it. Virtual
    /// strips list Windows capture endpoints, so a saved USB interface or
    /// virtual cable reopens too. `VBAN:` devices are bindings rather than
    /// Windows endpoints and wait for their stream.
    fn apply_saved_inputs(&mut self) {
        // Never enumerate here: this runs on the UI thread during start-up, and
        // enumeration blocks for minutes when a device endpoint is wedged. The
        // probe thread owns that call; an empty list means "not answering", so
        // leave the captures closed until it does.
        if self.last_inputs.is_empty() {
            diag_log("startup: capture devices unavailable, leaving inputs closed");
            return;
        }
        let available = self.last_inputs.clone();
        for i in 0..self.active_strips {
            let name = self.strips[i].device.clone();
            self.strips[i].tape_feed = i == 3 && name == "Virtual 1 (VAIO)";
            let is_vban = name.starts_with("VBAN: ");
            if matches!(self.captures[i], Some(StripInput::Vban(_))) {
                let matches = self
                    .captures[i]
                    .as_ref()
                    .map(|c| c.device().eq_ignore_ascii_case(&name))
                    .unwrap_or(false);
                if !is_vban || !matches {
                    self.vban.unbind(i);
                    self.captures[i] = None;
                    self.audio.detach_input(i);
                }
            }
            if is_vban {
                if self
                    .captures[i]
                    .as_ref()
                    .map(|c| c.device().eq_ignore_ascii_case(&name))
                    .unwrap_or(false)
                {
                    continue;
                }
                if let Some((stream, ip, port)) = Self::parse_vban_device(&name) {
                    self.set_vban_input(i, &stream, &ip, port, self.vban_quality);
                }
                continue;
            }
            if name == "No input"
                || name == "No input device found"
                || name == "No device"
                || name == "Virtual 1 (VAIO)"
                || name == "Virtual 2 (AUX)"
            {
                self.vban.unbind(i);
                self.captures[i] = None;
                self.audio.detach_input(i);
                self.strips[i].meter = 0.0;
                self.strips[i].peak = 0.0;
                continue;
            }
            if self
                .captures[i]
                .as_ref()
                .map(|c| c.device().eq_ignore_ascii_case(&name))
                .unwrap_or(false)
            {
                continue;
            }
            if !available.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
                continue;
            }
            // A cable endpoint that still exists from the previous session is
            // about to be unplugged and replugged. Opening it now yields a
            // stream that is "running" but permanently silent, so wait for the
            // attach to finish and let the retry open the fresh endpoint.
            if self.vc_cfg.enabled
                && cable_label(&name).is_some()
                && !self.vc.status().attached
            {
                continue;
            }
            let capture = InputCapture::open(Some(&name));
            if capture.available {
                diag_log(format!("input {}: opened '{}'", i + 1, capture.device));
                self.strips[i].device = capture.device.clone();
                self.audio.attach_input(i, capture.shared());
                self.captures[i] = Some(StripInput::Cpal(capture));
            } else {
                self.captures[i] = None;
                self.audio.detach_input(i);
                self.strips[i].meter = 0.0;
                self.strips[i].peak = 0.0;
            }
        }
    }

    /// The flat `key=value` file the earlier build wrote.
    fn load_settings_legacy(&mut self) {
        let text = match std::fs::read_to_string("mixer_settings.txt") {
            Ok(t) => t,
            Err(_) => {
                self.toast("No saved settings found");
                return;
            }
        };
        let mut tape_path = String::new();
        let mut tape_demo: i32 = -1;
        let mut recent: Vec<(usize, PathBuf)> = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if key == "sample_rate" {
                if let Ok(v) = value.parse() {
                    self.sample_rate = v;
                }
            } else if key == "buffer" {
                if let Ok(v) = value.parse() {
                    self.buffer = v;
                }
            } else if key == "main_buffer" {
                // FUN_00527ad0 rejects a main buffer above 2048 samples.
                if let Ok(v) = value.parse::<u32>() {
                    self.main_buffer = v.min(MAIN_BUFFER_MAX);
                }
            } else if key == "engine" {
                if let Ok(v) = value.parse::<usize>() {
                    self.engine_mode = v.min(1);
                }
            } else if key == "limit_remote" {
                self.limit_remote = value == "1";
            } else if key == "lock" {
                self.lock_gui = value == "1";
            } else if key == "AutoRestart" {
                self.auto_restart_a1 = value == "1";
            } else if key == "AutoRestartAll" {
                self.auto_restart_all = value == "1";
            } else if key == "tape.gain" {
                if let Ok(v) = value.parse() {
                    self.tape.gain_db = v;
                }
            } else if key == "tape.file" {
                self.tape.file = value.to_string();
            } else if key == "tape.path" {
                tape_path = value.to_string();
            } else if key == "tape.demo" {
                tape_demo = value.parse().unwrap_or(-1);
            } else if key == "tape.loop" {
                self.tape.loop_playback = value == "1";
            } else if let Some(idx_s) = key.strip_prefix("tape.recent") {
                if let Ok(idx) = idx_s.parse::<usize>() {
                    if !value.is_empty() {
                        recent.push((idx, PathBuf::from(value)));
                    }
                }
            } else if let Some(rest) = key.strip_prefix("strip") {
                let Some((idx_s, field)) = rest.split_once('.') else {
                    continue;
                };
                let Ok(idx) = idx_s.parse::<usize>() else {
                    continue;
                };
                if idx >= 5 {
                    continue;
                }
                let s = &mut self.strips[idx];
                match field {
                    // FUN_0052b490 stores the loaded dblevel after atof only;
                    // unlike every other field it is not clamped here.
                    "gain" => s.gain_db = value.parse().unwrap_or(s.gain_db),
                    "pan" => {
                        // FUN_0052b490 clamps the pan/panel x axis to
                        // -0.5..+0.5 (0x56dbe8 / 0x56dbe4).
                        s.pan = value.parse().unwrap_or(s.pan).clamp(PAN_MIN, PAN_MAX);
                        s.intelli[0] = s.pan;
                    }
                    // audibility_c / audibility_g at +0x34 / +0x38, 0..10.
                    "comp" => {
                        s.comp = value.parse().unwrap_or(s.comp).clamp(AUDIBILITY_MIN, AUDIBILITY_MAX)
                    }
                    "gate" => {
                        s.gate = value.parse().unwrap_or(s.gate).clamp(AUDIBILITY_MIN, AUDIBILITY_MAX)
                    }
                    "limit" => s.limit = value.parse().unwrap_or(s.limit).clamp(LIMIT_MIN, GAIN_MAX),
                    "mute" => s.mute = value == "1",
                    "solo" => s.solo = value == "1",
                    "mono" => s.mono = value == "1",
                    // FUN_0052b490 forces muc off for strip index 4 (the
                    // fifth strip, i.e. virtual strip 2): `param_1 == 4`.
                    "mc" => s.mc = value == "1" && idx != 4,
                    // FUN_0052b490 clamps the loaded karaoke value to 0..4.
                    "k" => s.karaoke = value.parse().unwrap_or(s.karaoke).min(4),
                    "device" => s.device = value.to_string(),
                    "eq" => {
                        let vals: Vec<f32> = value.split(',').filter_map(|p| p.parse().ok()).collect();
                        if vals.len() == 3 {
                            s.eq = [
                                vals[0].clamp(EQ_GAIN_MIN, EQ_GAIN_MAX),
                                vals[1].clamp(EQ_GAIN_MIN, EQ_GAIN_MAX),
                                vals[2].clamp(EQ_GAIN_MIN, EQ_GAIN_MAX),
                            ];
                        }
                    }
                    "panel" => {
                        // The active panel pair selected by paneltype. X
                        // clamps -0.5..+0.5; the hardware 3D panel clamps Y
                        // to 0..1 (FUN_00541ce0 controls 0x114-0x116), while
                        // a virtual pad clamps both axes to -0.5..+0.5.
                        let vals: Vec<f32> =
                            value.split(',').filter_map(|p| p.parse().ok()).collect();
                        if vals.len() == 2 {
                            let (ymin, ymax) = if s.kind == StripKind::Hardware {
                                (PANEL3D_Y_MIN, PANEL3D_Y_MAX)
                            } else {
                                (PANEL_Y_MIN, PANEL_Y_MAX)
                            };
                            let pair = [
                                vals[0].clamp(PANEL_X_MIN, PANEL_X_MAX),
                                vals[1].clamp(ymin, ymax),
                            ];
                            match s.panel_type {
                                1 => s.color_panel = pair,
                                2 => s.mod_panel = pair,
                                _ => {
                                    s.intelli = pair;
                                    s.pan = pair[0];
                                }
                            }
                        }
                    }
                    "paneltype" => {
                        s.panel_type = match value.parse::<i32>().unwrap_or(0) {
                            1 => 1,
                            2 => 2,
                            _ => 0,
                        };
                    }
                    "route" => {
                        let vals: Vec<u8> =
                            value.split(',').filter_map(|p| p.parse().ok()).collect();
                        if vals.len() == 5 || vals.len() == audio::MAX_BUSES {
                            for j in 0..audio::MAX_BUSES {
                                s.route[j] = vals.get(j).copied().unwrap_or(0) == 1;
                            }
                        }
                    }
                    _ => {}
                }
            } else if let Some(rest) = key.strip_prefix("bus") {
                let Some((idx_s, field)) = rest.split_once('.') else {
                    continue;
                };
                let Ok(idx) = idx_s.parse::<usize>() else {
                    continue;
                };
                if idx >= audio::MAX_BUSES {
                    continue;
                }
                let b = &mut self.buses[idx];
                match field {
                    // FUN_0052c030 stores the Bus dblevel after atof only, with
                    // no clamp (the Strip loader behaves the same way).
                    "gain" => b.gain_db = value.parse().unwrap_or(b.gain_db),
                    "mute" => b.mute = value == "1",
                    "mono" => b.mono = value.parse().unwrap_or(b.mono),
                    "eq" => b.eq_on = value == "1",
                    "sel" => b.sel = value == "1",
                    // FUN_0052c030 does BusMode = (value & 0xf) << 4, so the
                    // stored nibble is always 0..15 (negative wraps too).
                    "mode" => b.mode = value.parse::<usize>().unwrap_or(b.mode) % 16,
                    "device" => b.device = value.to_string(),
                    _ => {}
                }
            }
        }
        if tape_demo >= 0 {
            if let Some(kind) = DemoKind::from_index(tape_demo as usize) {
                self.select_demo(kind);
            }
        } else if !tape_path.is_empty() {
            if Path::new(&tape_path).exists() {
                self.load_wav_file(PathBuf::from(&tape_path));
            } else {
                self.tape.path = tape_path;
                self.tape_error = Some(format!(
                    "The saved file is no longer on disk: {}",
                    self.tape.path
                ));
            }
        }
        recent.sort_by_key(|(i, _)| *i);
        self.tape_recent = recent.into_iter().map(|(_, p)| p).take(6).collect();
        self.toast("Settings loaded from mixer_settings.txt");
    }

    fn reset_all(&mut self) {
        self.strips = (0..MAX_STRIPS).map(Strip::new).collect();
        self.captures = (0..MAX_STRIPS).map(|_| None).collect();
        self.active_strips = BASE_STRIPS;
        self.strip_bank = 0;
        self.vban.unbind_all();
        for i in 0..MAX_STRIPS {
            self.audio.detach_input(i);
        }
        self.audio.set_strip_count(BASE_STRIPS);
        self.buses = (0..audio::MAX_BUSES).map(Bus::new).collect();
        self.audio.unload();
        self.tape = Tape::new();
        self.tape_error = None;
        self.tape_info = None;
        self.vst_panel = None;
        self.picker = None;
        self.label_edit = None;
        self.pending_auto_assign.clear();
        self.toast("Settings reset to default");
    }

    // -----------------------------------------------------------------------
    // Top bar
    // -----------------------------------------------------------------------

    fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let p = ui.painter();
        p.rect_filled(rect, 0.0, Color32::from_rgb(20, 23, 29));
        p.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, Color32::from_rgb(45, 52, 63)),
        );

        // small logo mark (neutral shape, not the original artwork)
        let logo = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.top() + 6.0), Vec2::splat(26.0));
        p.rect_filled(logo, 5.0, AMBER);
        p.rect_stroke(logo, 5.0, Stroke::new(1.0, Color32::from_rgb(255, 205, 120)));
        p.text(
            logo.center(),
            Align2::CENTER_CENTER,
            "AM",
            FontId::proportional(10.0),
            Color32::from_rgb(30, 22, 10),
        );

        // The original has no title bar at all (window style 0x82020000, a
        // borderless popup). Its whole top strip is the move handle: the hit
        // test FUN_0053d9a0 returns control 110 for any client y < 0x23, and
        // FUN_0053e7b0 / FUN_004b2500 start the window move from there. The
        // buttons on the strip keep their own clicks.
        let menu_hit = Rect::from_min_size(Pos2::new(rect.left() + 166.0, rect.top() + 7.0), Vec2::new(64.0, 24.0));
        let vban_hit = Rect::from_min_size(Pos2::new(rect.left() + 234.0, rect.top() + 7.0), Vec2::new(56.0, 24.0));
        let mut bx = rect.right() - 8.0;
        let close_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 28.0;
        let min_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 28.0;
        let max_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 28.0;
        let help_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 28.0;
        let lock_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 30.0;
        let r_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        bx -= 28.0;
        let a_hit = Rect::from_min_size(Pos2::new(bx - 24.0, rect.top() + 7.0), Vec2::splat(24.0));
        let cables_hit = Rect::from_min_size(
            Pos2::new(rect.left() + 420.0, rect.top() + 7.0),
            Vec2::new(70.0, 24.0),
        );
        // Ducking sits next to CABLES: both are processing panels for the
        // virtual devices rather than scene/window controls.
        let ducking_hit = Rect::from_min_size(
            Pos2::new(rect.left() + 494.0, rect.top() + 7.0),
            Vec2::new(76.0, 24.0),
        );
        let button_hits = [
            menu_hit, vban_hit, cables_hit, ducking_hit, close_hit, min_hit, max_hit, help_hit,
            lock_hit, r_hit, a_hit,
        ];
        let drag_strip = Rect::from_min_max(
            rect.min,
            Pos2::new(rect.right(), rect.bottom().min(rect.top() + 35.0)),
        );
        let over_button = ui
            .ctx()
            .pointer_hover_pos()
            .map_or(false, |q| button_hits.iter().any(|b| b.expand(2.0).contains(q)));
        let drag = ui.interact(drag_strip, Id::new("window_drag_strip"), Sense::click_and_drag());
        if drag.hovered() && !over_button {
            ui.ctx().set_cursor_icon(if drag.dragged() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
        if drag.drag_started() && !over_button {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        // Start the drag on the press as well: if a native resize left the
        // pointer state stale, waiting for drag_started can miss the gesture.
        if drag.contains_pointer()
            && !over_button
            && ui.input(|i| i.pointer.primary_pressed())
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        let title_rect = Rect::from_min_size(Pos2::new(rect.left() + 42.0, rect.top() + 5.0), Vec2::new(120.0, 28.0));
        p.text(
            title_rect.left_top() + Vec2::new(0.0, 2.0),
            Align2::LEFT_TOP,
            "AUDIO MIXER",
            FontId::proportional(14.0),
            TEXT,
        );
        p.text(
            title_rect.left_top() + Vec2::new(0.0, 16.0),
            Align2::LEFT_TOP,
            "PRO CONSOLE",
            FontId::proportional(8.0),
            AMBER,
        );

        let menu_rect = Rect::from_min_size(Pos2::new(rect.left() + 166.0, rect.top() + 7.0), Vec2::new(64.0, 24.0));
        if top_button(ui, menu_rect, "MENU", self.menu_open, AMBER) {
            self.menu_open = !self.menu_open;
            if self.menu_open {
                self.menu_ignore_click = true;
            }
        }

        let vban_rect = Rect::from_min_size(Pos2::new(rect.left() + 234.0, rect.top() + 7.0), Vec2::new(56.0, 24.0));
        if top_button(ui, vban_rect, "VBAN", self.show_vban, TEAL) {
            self.show_vban = true;
        }

        // preset / scene badge
        let badge = Rect::from_min_size(Pos2::new(rect.left() + 300.0, rect.top() + 9.0), Vec2::new(112.0, 20.0));
        p.rect_filled(badge, 3.0, Color32::from_rgb(25, 29, 36));
        p.text(
            badge.center(),
            Align2::CENTER_CENTER,
            format!("SCENE: {}", self.preset_active.to_uppercase()),
            FontId::proportional(8.0),
            DIM,
        );

        let cables_rect = cables_hit;
        if top_button(ui, cables_rect, "CABLES", self.show_vc, AMBER) {
            self.show_vc = true;
        }

        if top_button(ui, ducking_hit, "DUCKING", self.show_ducking, AMBER) {
            self.show_ducking = true;
        }

        // right-hand status boxes
        let mut x = rect.right() - 8.0;
        let close = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        if top_button(ui, close, "X", false, RED) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        x -= 28.0;
        let min = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        if top_button(ui, min, "_", false, BLUE) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        x -= 28.0;
        let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        if top_icon_button(ui, max_hit, maximized) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        x -= 28.0;
        let help = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        if top_button(ui, help, "?", self.help_caption, TEAL) {
            self.show_about = true;
        }
        x -= 28.0;
        let lock = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        if top_button(ui, lock, "L", self.lock_gui, YELLOW) {
            self.lock_gui = !self.lock_gui;
            let v = self.lock_gui;
            self.toast(if v { "Graphic interface locked" } else { "Graphic interface unlocked" });
        }
        x -= 30.0;
        let rbox = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        p.rect_filled(rbox, 3.0, Color32::from_rgb(24, 34, 44));
        p.rect_stroke(rbox, 3.0, Stroke::new(1.0, BLUE));
        p.text(rbox.center(), Align2::CENTER_CENTER, "R", FontId::proportional(10.0), BLUE);
        if ui
            .interact(rbox, Id::new("rbox"), Sense::click())
            .on_hover_text("Remote API: applications connected to the mixer")
            .clicked()
        {
            self.toast("Remote API: 1 control application connected");
        }
        x -= 28.0;
        let abox = Rect::from_min_size(Pos2::new(x - 24.0, rect.top() + 7.0), Vec2::new(24.0, 24.0));
        p.rect_filled(abox, 3.0, Color32::from_rgb(24, 40, 32));
        p.rect_stroke(abox, 3.0, Stroke::new(1.0, GREEN));
        p.text(abox.center(), Align2::CENTER_CENTER, "A", FontId::proportional(10.0), GREEN);
        if ui
            .interact(abox, Id::new("abox"), Sense::click())
            .on_hover_text("Audio callback: applications using the virtual inputs")
            .clicked()
        {
            self.toast("Audio callback: 2 client applications connected");
        }

        // transient status message
        if self.time < self.status_until && !self.status.is_empty() {
            // Keep clear of CABLES and DUCKING so a transient message never
            // hides the panel buttons next to them.
            let srect = Rect::from_min_size(
                Pos2::new(rect.left() + 578.0, rect.top() + 8.0),
                Vec2::new(258.0, 22.0),
            );
            p.rect_filled(srect, 3.0, Color32::from_rgb(36, 44, 38));
            p.text(
                srect.center(),
                Align2::CENTER_CENTER,
                fit_text(p, &self.status, 9.5, 250.0),
                FontId::proportional(9.5),
                Color32::from_rgb(190, 220, 180),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Input strips
    // -----------------------------------------------------------------------

    /// Cable a strip listens to, as the label the app list keys on
    /// ("Virtual Cable 03"). Any input strip can capture one of the cable
    /// microphones, so hardware strips get the list as well.
    fn strip_cable(&self, idx: usize) -> Option<String> {
        let strip = self.strips.get(idx)?;
        cable_label(&strip.device)
    }

    /// Tell the session worker which cables are on screen, then take the latest
    /// snapshot for this frame.
    fn sync_app_targets(&mut self) {
        if self.safe_mode || self.startup_step != StartupStep::Done {
            if !self.apps_targets.is_empty() {
                self.apps.set_targets(&[]);
                self.apps_targets.clear();
            }
            return;
        }
        let mut targets: Vec<String> = (0..self.active_strips.min(self.strips.len()))
            .filter_map(|idx| self.strip_cable(idx))
            .collect();
        targets.sort();
        targets.dedup();
        if targets != self.apps_targets {
            self.apps.set_targets(&targets);
            self.apps_targets = targets;
        }
        self.apps_rows = self.apps.snapshot();
    }

    /// Header plus up to three app rows, painted between the strip's EQ panel
    /// and its routing buttons. Returns the vertical space used.
    fn draw_apps_block(
        &mut self,
        ui: &egui::Ui,
        idx: usize,
        left: f32,
        y: f32,
        w: f32,
        cable: &str,
    ) -> f32 {
        let open = self.apps_open[idx];
        let rows: Vec<sessions::AppSession> =
            self.apps_rows.get(cable).cloned().unwrap_or_default();
        let count = rows.len();
        let header = Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, APPS_HEADER_H));
        ui.painter().rect_filled(header, 2.0, TRACK);
        ui.painter().rect_stroke(header, 2.0, Stroke::new(1.0, LINE));
        ui.painter().text(
            Pos2::new(header.left() + 4.0, header.center().y),
            Align2::LEFT_CENTER,
            if count == 0 {
                "APPS".to_string()
            } else {
                format!("APPS {count}")
            },
            FontId::proportional(6.4),
            if count > 0 { TEAL } else { FAINT },
        );
        ui.painter().text(
            Pos2::new(header.right() - 4.0, header.center().y),
            Align2::RIGHT_CENTER,
            if open { "v" } else { ">" },
            FontId::proportional(6.0),
            FAINT,
        );
        let hresp = ui.interact(header, Id::new(("apps_head", idx)), Sense::click());
        if hresp.clicked() {
            self.apps_open[idx] = !open;
        }
        let _ = hresp.on_hover_text(format!(
            "Applications playing into {cable}: click to {} the list",
            if open { "hide" } else { "show" }
        ));
        if !open {
            return APPS_HEADER_H;
        }

        let area = Rect::from_min_size(
            Pos2::new(left, y + APPS_HEADER_H),
            Vec2::new(w, APPS_ROW_H * APPS_VISIBLE_ROWS as f32),
        );
        ui.painter().rect_filled(area, 1.0, Color32::from_rgb(16, 19, 24));
        if rows.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "No app is using this cable",
                FontId::proportional(6.2),
                FAINT,
            );
            return APPS_BLOCK_H;
        }

        let scroll_resp = ui.interact(area, Id::new(("apps_area", idx)), Sense::hover());
        if scroll_resp.hovered() {
            let wheel = ui.input(|i| i.raw_scroll_delta.y);
            if wheel.abs() > 0.5 {
                let max_offset = count.saturating_sub(APPS_VISIBLE_ROWS);
                let offset = self.apps_scroll[idx] as i32 - wheel.signum() as i32;
                self.apps_scroll[idx] = offset.clamp(0, max_offset as i32) as usize;
            }
        }
        let offset = self.apps_scroll[idx].min(count.saturating_sub(APPS_VISIBLE_ROWS));

        for (slot, row) in rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(APPS_VISIBLE_ROWS)
        {
            let ry = y + APPS_HEADER_H + (slot - offset) as f32 * APPS_ROW_H;
            let rect = Rect::from_min_size(Pos2::new(left, ry), Vec2::new(w, APPS_ROW_H));
            if row.peak > 0.001 && !row.muted {
                ui.painter()
                    .rect_filled(rect, 0.0, Color32::from_rgb(21, 28, 30));
            }

            let icon_rect = Rect::from_min_size(
                Pos2::new(rect.left() + 3.0, rect.top() + 4.0),
                Vec2::splat(13.0),
            );
            if let Some(icon) = &row.icon {
                let key = row
                    .exe
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("name:{}", row.name));
                if !self.app_icons.contains_key(&key) {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [icon.width as usize, icon.height as usize],
                        &icon.rgba,
                    );
                    let handle = ui.ctx().load_texture(
                        format!("appicon:{key}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.app_icons.insert(key.clone(), handle);
                }
                if let Some(handle) = self.app_icons.get(&key) {
                    ui.painter().image(
                        handle.id(),
                        icon_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
            } else {
                ui.painter()
                    .rect_filled(icon_rect, 2.0, Color32::from_rgb(46, 52, 62));
            }

            let mute_rect = Rect::from_min_size(
                Pos2::new(rect.right() - 16.0, rect.top() + 3.0),
                Vec2::new(14.0, 10.0),
            );
            let name_w = (mute_rect.left() - (icon_rect.right() + 3.0)).max(10.0);
            ui.painter().text(
                Pos2::new(icon_rect.right() + 3.0, rect.top() + 2.0),
                Align2::LEFT_TOP,
                fit_text(ui.painter(), &row.name, 7.0, name_w),
                FontId::proportional(7.0),
                if row.muted { FAINT } else { TEXT },
            );

            let mute_resp = ui.interact(
                mute_rect,
                Id::new(("app_mute", idx, row.pid)),
                Sense::click(),
            );
            ui.painter().rect_filled(
                mute_rect,
                2.0,
                if row.muted {
                    Color32::from_rgb(88, 32, 28)
                } else {
                    TRACK
                },
            );
            ui.painter().rect_stroke(
                mute_rect,
                2.0,
                Stroke::new(1.0, if row.muted { RED } else { LINE }),
            );
            ui.painter().text(
                mute_rect.center(),
                Align2::CENTER_CENTER,
                "M",
                FontId::proportional(6.4),
                if row.muted { TEXT } else { DIM },
            );
            if mute_resp.clicked() {
                self.apps.set_mute(cable, row.pid, !row.muted);
            }
            let _ = mute_resp.on_hover_text(format!(
                "{}: {} this application",
                row.name,
                if row.muted { "unmute" } else { "mute" }
            ));

            let bar = Rect::from_min_size(
                Pos2::new(icon_rect.right() + 3.0, rect.bottom() - 6.0),
                Vec2::new((rect.right() - 3.0 - icon_rect.right() - 3.0).max(8.0), 3.0),
            );
            ui.painter().rect_filled(bar, 1.0, Color32::from_rgb(38, 44, 54));
            ui.painter().rect_filled(
                Rect::from_min_size(
                    bar.left_top(),
                    Vec2::new(bar.width() * row.volume.clamp(0.0, 1.0), bar.height()),
                ),
                1.0,
                if row.muted { FAINT } else { TEAL.gamma_multiply(0.75) },
            );
            let peak_x = bar.left() + bar.width() * row.peak.sqrt().clamp(0.0, 1.0);
            ui.painter().line_segment(
                [
                    Pos2::new(peak_x, bar.top() - 1.0),
                    Pos2::new(peak_x, bar.bottom() + 1.0),
                ],
                Stroke::new(1.0, if row.muted { FAINT } else { TEXT }),
            );
            let vol_resp = ui.interact(
                bar.expand2(Vec2::new(0.0, 2.0)),
                Id::new(("app_vol", idx, row.pid)),
                Sense::click_and_drag(),
            );
            if vol_resp.dragged() || vol_resp.clicked() {
                if let Some(pos) = vol_resp.interact_pointer_pos() {
                    let fraction = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
                    self.apps.set_volume(cable, row.pid, fraction);
                }
            }
            let _ = vol_resp.on_hover_text(format!(
                "{} ({} session(s)): volume {:.0}%, peak {:.0}%\nDrag the bar to set the app volume",
                row.name,
                row.sessions,
                row.volume * 100.0,
                row.peak * 100.0
            ));
        }
        APPS_BLOCK_H
    }

    fn draw_strip_card(&mut self, ui: &egui::Ui, card: Rect, idx: usize) {
        let accent = if idx < 3 { Color32::from_rgb(128, 168, 190) } else { TEAL };
        let inner = card.shrink2(Vec2::new(5.0, 4.0));
        ui.painter().rect_filled(card, 5.0, PANEL);
        ui.painter().rect_stroke(card, 5.0, Stroke::new(1.0, LINE));
        if self.strips[idx].solo {
            ui.painter().rect_stroke(card, 5.0, Stroke::new(1.0, YELLOW));
        }
        if self.strips[idx].mute {
            let d = Rect::from_min_size(card.left_top() + Vec2::new(4.0, 4.0), Vec2::splat(5.0));
            ui.painter().circle_filled(d.center(), 2.5, RED);
        }

        let enabled = !self.lock_gui;
        let mut y = inner.top();
        let w = inner.width();

        // header (click = choose device, right click = strip menu)
        let header = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, 26.0));
        let hresp = ui.interact(header, Id::new(("strip_head", idx)), Sense::click());
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let vst_btn = Rect::from_min_size(
            Pos2::new(header.right() - 25.0, header.top() + 7.5),
            Vec2::new(24.0, 11.0),
        );
        let mut over_vst = false;
        {
            // Every input strip has its own VST3 insert. This chip chooses the
            // plug-in; the INSERT panel below opens its own screen.
            let vst_resp = ui.interact(vst_btn, Id::new(("vst_slot", idx)), Sense::click());
            over_vst = pointer.map_or(false, |p| vst_btn.contains(p));
            let loaded = self.vsts[idx].loaded();
            let fill = if loaded {
                if self.vsts[idx].bypass {
                    Color32::from_rgb(96, 78, 40)
                } else {
                    Color32::from_rgb(42, 92, 72)
                }
            } else {
                TRACK
            };
            ui.painter().rect_filled(vst_btn, 2.0, fill);
            ui.painter().rect_stroke(
                vst_btn,
                2.0,
                Stroke::new(1.0, if loaded { TEAL } else { LINE }),
            );
            ui.painter().text(
                vst_btn.center(),
                Align2::CENTER_CENTER,
                "VST",
                FontId::proportional(6.3),
                if loaded { TEXT } else { DIM },
            );
            let clicked = vst_resp.clicked();
            let _ = vst_resp.on_hover_text(format!(
                "VST3 plug-in slot on {}: click to choose a plug-in",
                self.strip_label(idx)
            ));
            if clicked && enabled {
                self.open_vst_chooser(idx);
            }
        }
        ui.painter().text(
            Pos2::new(header.left(), header.top() + 1.0),
            Align2::LEFT_TOP,
            fit_text(ui.painter(), &self.strips[idx].name, 9.5, w - 27.0),
            FontId::proportional(9.5),
            TEXT,
        );
        // A cable endpoint's full name ("Microphone (Virtual Cable 03)") never
        // fits the strip, so show the cable itself: that is the part the person
        // needs to know. The complete name stays in the hover text.
        let device_label = cable_label(&self.strips[idx].device)
            .unwrap_or_else(|| self.strips[idx].device.clone());
        ui.painter().text(
            Pos2::new(header.left(), header.top() + 13.0),
            Align2::LEFT_TOP,
            fit_text(ui.painter(), &device_label, 7.3, w - 27.0),
            FontId::proportional(7.3),
            DIM,
        );
        if hresp.clicked() && enabled && !over_vst {
            self.picker = Some(Picker::Strip(idx));
        }
        let _ = hresp.clone().on_hover_text(format!(
            "{}\nClick to choose the input device, right click for the strip menu",
            self.strips[idx].device
        ));
        if !self.lock_gui {
            let mut open_label = false;
            let mut reset_all = false;
            let mut reset_eq = false;
            let mut reset_pan = false;
            let mut reset_panel = false;
            let mut reset_comp = false;
            let mut reset_gate = false;
            let mut copy_all = false;
            let mut paste = false;
            let mut toggle_apps = false;
            let mut remove_last = false;
            let has_clip = self.strip_clip.is_some();
            hresp.context_menu(|ui| {
                if ui.button("Edit label...").clicked() {
                    open_label = true;
                    ui.close_menu();
                }
                // FUN_00538860 builds this popup: command 600 "Reset Strip",
                // then the block resets for the strip kind.
                if ui.button("Reset Strip").clicked() {
                    reset_all = true;
                    ui.close_menu();
                }
                ui.separator();
                if self.strip_cable(idx).is_some() {
                    ui.separator();
                    let label = if self.apps_open[idx] {
                        "Hide apps on this strip"
                    } else {
                        "Show apps on this strip"
                    };
                    if ui.button(label).clicked() {
                        toggle_apps = true;
                        ui.close_menu();
                    }
                }
                ui.separator();
                // Commands 0x262 (Copy All) and 0x26c (Paste).
                if ui.button("Copy All").clicked() {
                    copy_all = true;
                    ui.close_menu();
                }
                if ui
                    .add_enabled(has_clip, egui::Button::new("Paste"))
                    .clicked()
                {
                    paste = true;
                    ui.close_menu();
                }
                if idx >= BASE_STRIPS && idx + 1 == self.active_strips {
                    ui.separator();
                    if ui.button("Remove this virtual input").clicked() {
                        remove_last = true;
                        ui.close_menu();
                    }
                }
            });
            if open_label {
                self.label_buf = self.strips[idx].name.clone();
                self.label_edit = Some(idx);
            }
            if toggle_apps {
                self.apps_open[idx] = !self.apps_open[idx];
            }
            if reset_all {
                // FUN_00538b20 command 600: hardware resets the panel, the
                // compressor and the gate; virtual resets the pan pair and the
                // three EQ bands.
                if self.strips[idx].kind == StripKind::Hardware {
                    reset_panel = true;
                    reset_comp = true;
                    reset_gate = true;
                } else {
                    reset_pan = true;
                    reset_eq = true;
                }
            }
            if reset_eq {
                // FUN_00538b20 0x25b: EQ fields +0x24/+0x28/+0x2c only.
                self.strips[idx].eq = [0.0, 0.0, 0.0];
            }
            if reset_pan {
                // FUN_00538b20 0x261: virtual pan pair +0x08/+0x0c only.
                self.strips[idx].pan = 0.0;
                self.strips[idx].intelli = [0.0, 0.0];
            }
            if reset_panel {
                // FUN_00538b20 0x259: all six panel fields (+0x08..+0x1c).
                self.strips[idx].pan = 0.0;
                self.strips[idx].intelli = [0.0, 0.0];
                self.strips[idx].color_panel = [0.0, 0.0];
                self.strips[idx].mod_panel = [0.0, 0.0];
            }
            if reset_comp {
                // FUN_00541510: compressor +0x34 knob -> 0, flag bit 0x10
                // cleared, nine-dword preset at 0x584abc rewritten.
                self.strips[idx].comp = 0.0;
                self.strips[idx].comp_prg = false;
            }
            if reset_gate {
                // FUN_00541510: gate +0x38 knob -> 0, flag bit 0x20 cleared.
                self.strips[idx].gate = 0.0;
                self.strips[idx].gate_prg = false;
            }
            if copy_all {
                self.strip_clip = Some(StripClip::capture(&self.strips[idx]));
                self.toast(format!("Copied {}", self.strips[idx].name));
            }
            if paste {
                if let Some(clip) = self.strip_clip {
                    clip.apply(&mut self.strips[idx]);
                    self.toast(format!("Pasted into {}", self.strips[idx].name));
                }
            }
            if remove_last {
                self.remove_last_virtual_input();
                return;
            }
        }
        y += 29.0;

        // Plug-in insert panel. The built-in pan/comp/gate/3-band EQ controls are
        // gone: every strip runs a VST3 insert, so this space names the plug-in
        // that is actually in the chain and opens its own screen.
        let insert = Rect::from_min_size(Pos2::new(inner.left(), y + 2.0), Vec2::new(w, 92.0));
        let insert_resp = ui.interact(insert, Id::new(("insert_panel", idx)), Sense::click());
        {
            let loaded = self.vsts[idx].loaded();
            let bypassed = self.vsts[idx].bypass;
            let error = self.vsts[idx].error.clone();
            let border = if error.is_some() {
                RED
            } else if loaded {
                if bypassed {
                    AMBER
                } else {
                    TEAL
                }
            } else {
                LINE
            };
            ui.painter().rect_filled(insert, 3.0, TRACK);
            ui.painter().rect_stroke(insert, 3.0, Stroke::new(1.0, border));

            let (state, state_color) = if error.is_some() {
                ("ERROR", RED)
            } else if !loaded {
                ("EMPTY", FAINT)
            } else if bypassed {
                ("BYPASS", AMBER)
            } else {
                ("ACTIVE", TEAL)
            };
            ui.painter().text(
                Pos2::new(insert.right() - 5.0, insert.bottom() - 14.0),
                Align2::RIGHT_TOP,
                state,
                FontId::proportional(6.3),
                state_color,
            );

            let latency = if loaded {
                let (in_latency, out_latency) = self.vsts[idx].chain.latency();
                let samples = in_latency.max(out_latency);
                if samples > 0 {
                    format!(
                        "{:.1} ms latency",
                        (samples as f32 / self.sample_rate.max(1) as f32) * 1000.0
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            match self.vsts[idx].info.as_ref() {
                Some(info) => {
                    let vendor = format!("{} {} · {}", info.vendor, info.version, info.format);
                    ui.painter().text(
                        Pos2::new(insert.left() + 5.0, insert.top() + 6.0),
                        Align2::LEFT_TOP,
                        fit_text(ui.painter(), &info.name, 7.8, insert.width() - 10.0),
                        FontId::proportional(7.8),
                        TEXT,
                    );
                    let sub = &self.vsts[idx].sub_plugins;
                    if sub.is_empty() {
                        ui.painter().text(
                            Pos2::new(insert.left() + 5.0, insert.top() + 18.0),
                            Align2::LEFT_TOP,
                            fit_text(ui.painter(), &vendor, 6.4, insert.width() - 10.0),
                            FontId::proportional(6.4),
                            DIM,
                        );
                        if !latency.is_empty() {
                            ui.painter().text(
                                Pos2::new(insert.left() + 5.0, insert.top() + 28.0),
                                Align2::LEFT_TOP,
                                &latency,
                                FontId::proportional(6.4),
                                FAINT,
                            );
                        }
                    } else {
                        // Chain wrapper: list its plug-ins under the wrapper's
                        // name, small enough that six fit. A leading '~' marks a
                        // slot that is bypassed inside the wrapper.
                        const LINES: usize = 6;
                        let step = 8.8;
                        let shown = sub.len().min(if sub.len() > LINES { LINES - 1 } else { LINES });
                        for (line, entry) in sub.iter().take(shown).enumerate() {
                            let bypassed = entry.starts_with('~');
                            let name = entry.trim_start_matches('~');
                            // A bypassed slot keeps the dash but is drawn dim,
                            // with '~' in place of '-': one glyph, so the name
                            // still fits the narrow strips.
                            let text = format!(
                                "{} {name}",
                                if bypassed { "~" } else { "-" }
                            );
                            let y = insert.top() + 18.0 + step * line as f32;
                            ui.painter().text(
                                Pos2::new(insert.left() + 5.0, y),
                                Align2::LEFT_TOP,
                                fit_text(ui.painter(), &text, 6.1, insert.width() - 6.0),
                                FontId::proportional(6.1),
                                if bypassed { FAINT } else { DIM },
                            );
                        }
                        if sub.len() > shown {
                            ui.painter().text(
                                Pos2::new(insert.left() + 5.0, insert.top() + 18.0 + step * shown as f32),
                                Align2::LEFT_TOP,
                                format!("+{} more", sub.len() - shown),
                                FontId::proportional(6.1),
                                FAINT,
                            );
                        }
                    }
                }
                None => {
                    ui.painter().text(
                        Pos2::new(insert.left() + 5.0, insert.top() + 6.0),
                        Align2::LEFT_TOP,
                        "No plug-in",
                        FontId::proportional(7.8),
                        FAINT,
                    );
                    ui.painter().text(
                        Pos2::new(insert.left() + 5.0, insert.top() + 18.0),
                        Align2::LEFT_TOP,
                        fit_text(
                            ui.painter(),
                            "Click to choose one",
                            6.4,
                            insert.width() - 10.0,
                        ),
                        FontId::proportional(6.4),
                        DIM,
                    );
                }
            }
            if let Some(err) = error.as_ref().filter(|_| self.vsts[idx].sub_plugins.is_empty()) {
                ui.painter().text(
                    Pos2::new(insert.left() + 5.0, insert.top() + 41.0),
                    Align2::LEFT_TOP,
                    fit_text(ui.painter(), err, 6.2, insert.width() - 10.0),
                    FontId::proportional(6.2),
                    RED,
                );
            }

            let tip = match self.vsts[idx].info.as_ref() {
                Some(info) => format!(
                    "{}\n{} {}\n{}\n{} in / {} out{}\nClick: open its screen   Right click: {}",
                    info.name,
                    info.vendor,
                    info.version,
                    info.path.display(),
                    info.audio_inputs,
                    info.audio_outputs,
                    if latency.is_empty() {
                        String::new()
                    } else {
                        format!("\n{latency}")
                    },
                    if bypassed { "enable" } else { "bypass" }
                ),
                None => format!(
                    "No plug-in on {}\nClick to choose one",
                    self.strip_label(idx)
                ),
            };
            let _ = insert_resp.clone().on_hover_text(tip);

            if insert_resp.clicked() && enabled {
                if loaded {
                    self.open_vst_editor(idx);
                } else {
                    self.open_vst_chooser(idx);
                }
            }
            if insert_resp.secondary_clicked() && enabled && loaded {
                let bypass = !bypassed;
                self.vsts[idx].bypass = bypass;
                self.vsts[idx].chain.set_bypass(bypass);
            }

            let hint = if loaded { "Open" } else { "Choose" };
            ui.painter().text(
                Pos2::new(insert.left() + 5.0, insert.bottom() - 14.0),
                Align2::LEFT_TOP,
                fit_text(ui.painter(), hint, 6.0, insert.width() - 44.0),
                FontId::proportional(6.0),
                FAINT,
            );
        }
        y += 96.0;

        // Applications rendering into this strip's virtual cable. The block is
        // only there for virtual strips bound to a Virtual Cable endpoint.
        if let Some(cable) = self.strip_cable(idx) {
            y += self.draw_apps_block(ui, idx, inner.left(), y, w, &cable);
        }

        // routing buttons
        let g = 2.0;
        let bw = (w - 2.0 * g) / 3.0;
        for i in 0..3 {
            let r = Rect::from_min_size(
                Pos2::new(inner.left() + i as f32 * (bw + g), y),
                Vec2::new(bw, 16.0),
            );
            let route = self.strips[idx].route[i];
            let (clicked, _resp) = chip(
                ui,
                r,
                &format!("A{}", i + 1),
                route,
                AMBER,
                enabled,
                Id::new(("route", idx, i)),
                "Assign this source to the hardware bus",
            );
            if clicked {
                let next = !route;
                self.strips[idx].route[i] = next;
                self.announce_route(idx, i, next);
            }
        }
        let virtual_buses = (audio::MAX_BUSES - 3) as f32;
        let bw2 = (w - g * (virtual_buses - 1.0)) / virtual_buses;
        for i in 3..audio::MAX_BUSES {
            let r = Rect::from_min_size(
                Pos2::new(inner.left() + (i - 3) as f32 * (bw2 + g), y + 18.0),
                Vec2::new(bw2, 16.0),
            );
            let route = self.strips[idx].route[i];
            let (clicked, _resp) = chip(
                ui,
                r,
                &format!("B{}", i - 2),
                route,
                BLUE,
                enabled,
                Id::new(("route", idx, i)),
                "Assign this source to the virtual bus",
            );
            if clicked {
                let next = !route;
                self.strips[idx].route[i] = next;
                self.announce_route(idx, i, next);
            }
        }
        y += 38.0;

        // toggle buttons
        let bw = (w - 2.0 * g) / 3.0;
        let mutes = [
            ("MUTE", self.strips[idx].mute, RED),
            ("SOLO", self.strips[idx].solo, YELLOW),
            ("MONO", self.strips[idx].mono, BLUE),
        ];
        for (i, (label, active, col)) in mutes.iter().enumerate() {
            let r = Rect::from_min_size(
                Pos2::new(inner.left() + i as f32 * (bw + g), y),
                Vec2::new(bw, 16.0),
            );
            let (clicked, _resp) = chip(
                ui,
                r,
                label,
                *active,
                *col,
                enabled,
                Id::new(("tog", idx, i)),
                match *label {
                    "MUTE" => "Switch this audio source off",
                    "SOLO" => "Listen to this source only (others are muted)",
                    _ => "L = R = L + R",
                },
            );
            if clicked {
                match i {
                    0 => self.strips[idx].mute = !self.strips[idx].mute,
                    1 => self.strips[idx].solo = !self.strips[idx].solo,
                    _ => self.strips[idx].mono = !self.strips[idx].mono,
                }
            }
        }
        // Centre-channel mute lives on the first virtual strip; karaoke on the
        // second one (strings VInMuteC1 = M.C. Virtual Strip #4, VInMuteC2 =
        // K Button Virtual Strip #5). Hardware inputs have neither button.
        if idx == 3 {
            let mc = self.strips[idx].mc;
            let (c1, _) = chip(
                ui,
                Rect::from_min_size(Pos2::new(inner.left(), y + 18.0), Vec2::new(w, 16.0)),
                "M.C.",
                mc,
                PURPLE,
                enabled,
                Id::new(("mc", idx)),
                "Mute the centre channel (dialog) of a multichannel source",
            );
            if c1 {
                self.strips[idx].mc = !mc;
            }
        } else if idx == 4 {
            let k = self.strips[idx].karaoke;
            let label = if k == 0 {
                "K".to_string()
            } else {
                format!("K{}", k)
            };
            let (c2, _) = chip(
                ui,
                Rect::from_min_size(Pos2::new(inner.left(), y + 18.0), Vec2::new(w, 16.0)),
                &label,
                k > 0,
                TEAL,
                enabled,
                Id::new(("karaoke", idx)),
                "Karaoke: removes voice/centre; click cycles the processing",
            );
            if c2 {
                // FUN_0053e7b0 (K button, control 0x515) cycles 0..4 and
                // FUN_0052b490 clamps the stored karaoke value to 0..4.
                self.strips[idx].karaoke = (k + 1) % 5;
            }
        }
        y += 38.0;

        let ducked = self.duck.enabled
            && self.duck.source.is_some_and(|src| src < self.active_strips)
            && (self.duck.targets & (1u32 << idx)) != 0
            && self.audio.duck_gain_db() < -0.5;
        if ducked {
            let chip_rect =
                Rect::from_min_size(Pos2::new(inner.right() - 25.0, y), Vec2::new(25.0, 8.0));
            ui.painter()
                .rect_filled(chip_rect, 1.5, AMBER.gamma_multiply(0.85));
            ui.painter().text(
                chip_rect.center(),
                Align2::CENTER_CENTER,
                "DUCK",
                FontId::proportional(5.6),
                Color32::from_rgb(26, 20, 10),
            );
        }
        y += 12.0;

        // fader + meter
        let bottom = inner.bottom() - 15.0;
        let fh = (bottom - y).max(40.0);
        let area = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, fh));
        let fader_w = 40.0;
        let pair_w = fader_w + 3.0 + 9.0;
        let fx = area.center().x - pair_w * 0.5;
        let fader_rect = Rect::from_min_size(Pos2::new(fx, area.top()), Vec2::new(fader_w, fh));
        let meter_rect = Rect::from_min_size(
            Pos2::new(fader_rect.right() + 4.0, area.top()),
            Vec2::new(9.0, fh),
        );
        let mut gain = self.strips[idx].gain_db;
        let mut limit = self.strips[idx].limit;
        let meter = self.strips[idx].meter;
        let peak = self.strips[idx].peak;
        let max_db = if self.limit_remote { 0.0 } else { GAIN_MAX };
        fader(
            ui,
            fader_rect,
            &mut gain,
            meter,
            peak,
            &mut limit,
            accent,
            Id::new(("gfader", idx)),
            enabled,
            true,
            max_db,
            self.streamer_brightness(idx),
            "Strip gain: drag, wheel steps 3 dB, double click for 0 dB",
        );
        meter_widget(
            ui,
            meter_rect,
            meter,
            peak,
            &mut limit,
            Id::new(("smeter", idx)),
            enabled,
            METER_MIN_DB,
            METER_SPAN_DB,
            STRIP_FADER_PAD,
        );
        self.strips[idx].gain_db = gain.clamp(GAIN_MIN, max_db);
        self.strips[idx].limit = limit.clamp(LIMIT_MIN, GAIN_MAX);
        ui.painter().text(
            Pos2::new(area.center().x, area.bottom() + 2.0),
            Align2::CENTER_TOP,
            fmt_db(self.strips[idx].gain_db),
            FontId::proportional(7.5),
            if self.strips[idx].mute { FAINT } else { TEXT },
        );
    }

    // -----------------------------------------------------------------------
    // Output buses
    // -----------------------------------------------------------------------

    /// Cable number a bus plays into, including the automatic wiring of an
    /// unset B1/B2. `None` when the bus has nowhere to go.
    fn bus_cable_number(&self, bus: usize) -> Option<u32> {
        let device = self.buses.get(bus)?.device.trim();
        if !device.is_empty() {
            return cable_number(device);
        }
        match bus {
            3 if self.driver_mode || self.vc_cfg.enabled => Some(1),
            4 if self.driver_mode || self.vc_cfg.enabled => Some(2),
            _ => None,
        }
    }

    /// Strips that would hear their own bus: a strip capturing cable N that is
    /// routed to a bus playing into cable N closes the ring, which is what
    /// turns into echo and then howl.
    fn feedback_loops(&self) -> [Vec<usize>; audio::MAX_BUSES] {
        let bus_cables: [Option<u32>; audio::MAX_BUSES] =
            std::array::from_fn(|bus| self.bus_cable_number(bus));
        let active = self.strips.len().min(self.active_strips);
        let strip_cables: Vec<Option<u32>> = self.strips[..active]
            .iter()
            .map(|strip| cable_number(&strip.device))
            .collect();
        let routes: Vec<[bool; audio::MAX_BUSES]> =
            self.strips[..active].iter().map(|strip| strip.route).collect();
        feedback_loops_from(&bus_cables, &strip_cables, &routes)
    }

    fn draw_bus_card(&mut self, ui: &egui::Ui, card: Rect, idx: usize, loop_strips: &[usize]) {
        let accent = if idx < 3 { AMBER } else { BLUE };
        let looping = !loop_strips.is_empty();
        let loop_names = loop_strips
            .iter()
            .map(|&strip| self.strip_label(strip))
            .collect::<Vec<_>>()
            .join(", ");
        if looping && !self.loop_warned[idx] {
            self.loop_warned[idx] = true;
            self.toast(format!(
                "BUS {}: feedback loop - {loop_names} listens to this cable and feeds it back",
                self.buses[idx].name
            ));
        } else if !looping {
            self.loop_warned[idx] = false;
        }
        let inner = card.shrink2(Vec2::new(5.0, 4.0));
        ui.painter().rect_filled(card, 5.0, Color32::from_rgb(24, 27, 33));
        ui.painter().rect_stroke(card, 5.0, Stroke::new(1.0, LINE));
        if self.buses[idx].sel {
            ui.painter().rect_stroke(card, 5.0, Stroke::new(1.0, accent.gamma_multiply(0.8)));
        }

        let enabled = !self.lock_gui;
        let w = inner.width();
        let mut y = inner.top();

        let header = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, 26.0));
        let hresp = ui.interact(header, Id::new(("bus_head", idx)), Sense::click());
        // Status stripe on the card's left edge: green while the stream runs,
        // amber while it is being opened, red after a failed open (retried
        // every three seconds), dim when the bus has no device. The hover text
        // carries the last error.
        let state = self.audio.bus_state(idx);
        let (dot_color, state_text) = match state {
            audio::BusState::Running => (GREEN, "output running".to_string()),
            audio::BusState::Opening => (AMBER, "opening the output ...".to_string()),
            audio::BusState::Failed => (
                RED,
                format!(
                    "output failed: {}",
                    self.audio.bus_error(idx).unwrap_or("unknown error")
                ),
            ),
            audio::BusState::Off => (DIM, "no output device".to_string()),
        };
        ui.painter().rect_filled(
            Rect::from_min_size(
                Pos2::new(card.left() + 2.0, card.top() + 4.0),
                Vec2::new(3.0, 22.0),
            ),
            1.5,
            dot_color,
        );
        ui.painter().text(
            Pos2::new(header.left(), header.top() + 1.0),
            Align2::LEFT_TOP,
            fit_text(ui.painter(), &format!("BUS {}", self.buses[idx].name), 10.0, w),
            FontId::proportional(10.0),
            accent,
        );
        ui.painter().text(
            Pos2::new(header.left(), header.top() + 13.0),
            Align2::LEFT_TOP,
            fit_text(
                ui.painter(),
                &if !self.buses[idx].device.trim().is_empty() {
                    self.buses[idx].device.clone()
                } else if idx == 3 || idx == 4 {
                    // Unset B1/B2 keep their automatic wiring; say which cable
                    // that is so the empty state is not read as "off".
                    if self.driver_mode {
                        format!("Virtual Cable {:02} (auto)", idx - 2)
                    } else {
                        format!("USB Cable {:02} (auto)", idx - 2)
                    }
                } else {
                    "No device".to_string()
                },
                7.3,
                w + 2.0,
            ),
            FontId::proportional(7.3),
            if looping { RED } else { DIM },
        );
        // A bus that would hear its own output gets a red badge next to REF;
        // hovering the card explains which input closes the ring.
        if looping {
            let chip = Rect::from_min_size(
                Pos2::new(card.right() - 64.0, card.top() + 3.0),
                Vec2::new(32.0, 13.0),
            );
            ui.painter().rect_filled(chip, 3.0, Color32::from_rgb(70, 20, 24));
            ui.painter()
                .rect_stroke(chip, 3.0, Stroke::new(1.0, RED));
            ui.painter().text(
                chip.center(),
                Align2::CENTER_CENTER,
                "LOOP",
                FontId::proportional(7.0),
                RED,
            );
        }
        // Small REF chip in the header's right corner: re-scan the device list
        // and reopen this bus, so a device connected after the last probe
        // (Bluetooth headphones, a USB interface still enumerating) shows up
        // without switching the bus to another device and back.
        let refresh_rect = Rect::from_min_size(
            Pos2::new(card.right() - 30.0, card.top() + 3.0),
            Vec2::new(26.0, 13.0),
        );
        let rresp = ui.interact(
            refresh_rect,
            Id::new(("bus_refresh", idx)),
            Sense::click(),
        );
        let rhover = rresp.hovered();
        ui.painter()
            .rect_filled(refresh_rect, 3.0, if rhover { PANEL_HI } else { TRACK });
        ui.painter()
            .rect_stroke(refresh_rect, 3.0, Stroke::new(1.0, LINE));
        ui.painter().text(
            refresh_rect.center(),
            Align2::CENTER_CENTER,
            "REF",
            FontId::proportional(7.0),
            if rhover { TEXT } else { DIM },
        );
        let refresh_clicked = rresp.clicked();
        let _ = rresp.on_hover_text("Re-scan audio devices for this bus");
        if refresh_clicked && enabled {
            self.audio.refresh_bus_outputs();
            // Trigger the device probe on the next poll instead of waiting for
            // the one-second timer.
            self.poll_clock = 1.0;
            self.toast(format!(
                "BUS {}: re-scanning audio devices",
                self.buses[idx].name
            ));
        }

        let over_refresh = ui
            .input(|i| i.pointer.interact_pos())
            .map(|pos| refresh_rect.contains(pos))
            .unwrap_or(false);
        if hresp.clicked() && enabled && !over_refresh {
            self.picker = Some(Picker::Bus(idx));
        }
        if hresp.double_clicked() && enabled {
            self.bus_rename = Some(idx);
            self.bus_rename_text = self.buses[idx].name.clone();
            self.bus_rename_focus = true;
        }
        let mut hover = if looping {
            format!(
                "FEEDBACK LOOP: {loop_names} captures this cable and is routed back into it, \
                 so the bus feeds itself. Change that input or this bus's device."
            )
        } else if idx == 3 || idx == 4 {
            "Click to choose the output device for this bus; leave it unset to use the \
             automatic virtual cable. Double-click the title to rename the bus."
                .to_string()
        } else {
            "Click to choose the output device for this bus. Double-click the title to rename it."
                .to_string()
        };
        hover.push_str("\n\n");
        hover.push_str(&state_text);
        let _ = hresp.on_hover_text(hover);
        y += 29.0;

        // bus mode selector
        let mode_rect = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, 18.0));
        let (mclick, mresp) = mode_chip(
            ui,
            mode_rect,
            MODE_NAMES[mode_slot(self.buses[idx].mode)],
            accent,
            enabled,
            Id::new(("mode", idx)),
            MODE_HELP[mode_slot(self.buses[idx].mode)],
        );
        if mclick {
            // Stored BusMode values are not visited in numeric order by the
            // original button (see next_bus_mode / FUN_0053e7b0).
            self.buses[idx].mode = next_bus_mode(self.buses[idx].mode);
            self.toast(format!(
                "BUS {} mode: {}",
                self.buses[idx].name,
                MODE_NAMES[mode_slot(self.buses[idx].mode)]
            ));
        }
        mresp.context_menu(|ui| {
            ui.label("Bus mode");
            for (i, name) in MODE_NAMES.iter().enumerate() {
                if ui
                    .selectable_label(self.buses[idx].mode == i, format!("{i}. {name}"))
                    .clicked()
                {
                    self.buses[idx].mode = i;
                    ui.close_menu();
                }
            }
        });
        y += 22.0;

        // buttons
        let g = 2.0;
        let bw = (w - g) / 2.0;
        let b = &self.buses[idx];
        let (c, resp) = chip(
            ui,
            Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(bw, 17.0)),
            "EQ",
            b.eq_on,
            accent,
            enabled,
            Id::new(("beq", idx)),
            "Master 6-band parametric EQ: click on/off, right click to open",
        );
        if c {
            self.buses[idx].eq_on = !b.eq_on;
        }
        let mut open_eq = false;
        let mut flat = false;
        resp.context_menu(|ui| {
            if ui.button("Open 6-band EQ...").clicked() {
                open_eq = true;
                ui.close_menu();
            }
            if ui.button("Flat EQ").clicked() {
                flat = true;
                ui.close_menu();
            }
        });
        if open_eq {
            self.bus_eq_open = Some(idx);
        }
        if flat {
            for band in &mut self.buses[idx].eq {
                band.gain = 0.0;
            }
            self.toast(format!("BUS {} EQ flattened", self.buses[idx].name));
        }
        let (c, _) = chip(
            ui,
            Rect::from_min_size(Pos2::new(inner.left() + bw + g, y), Vec2::new(bw, 17.0)),
            "SEL",
            self.buses[idx].sel,
            BLUE,
            enabled,
            Id::new(("bsel", idx)),
            "Select this bus",
        );
        if c {
            self.buses[idx].sel = !self.buses[idx].sel;
            if self.monitor_on_sel {
                self.buses[idx].monitor = self.buses[idx].sel;
            }
        }
        // FUN_0053e7b0 cases 0x125-0x129 read bits 2 (0x4, mono) and 9
        // (0x200, L/R exchange) of the bus flag word. Pressing MONO walks
        // 0x00 -> 0x04 -> 0x200 -> 0x00, i.e. off, mono, swapped, off.
        let mono_label = match self.buses[idx].mono {
            0 => "MONO",
            1 => "MONO+",
            _ => "SWAP",
        };
        let (c, _) = chip(
            ui,
            Rect::from_min_size(Pos2::new(inner.left(), y + 19.0), Vec2::new(bw, 17.0)),
            mono_label,
            self.buses[idx].mono > 0,
            TEAL,
            enabled,
            Id::new(("bmono", idx)),
            "Mono: L = R = L+R; click again to exchange L and R",
        );
        if c {
            self.buses[idx].mono = (self.buses[idx].mono + 1) % 3;
        }
        let (c, _) = chip(
            ui,
            Rect::from_min_size(
                Pos2::new(inner.left() + bw + g, y + 19.0),
                Vec2::new(bw, 17.0),
            ),
            "MUTE",
            self.buses[idx].mute,
            RED,
            enabled,
            Id::new(("bmute", idx)),
            "Switch this output bus off",
        );
        if c {
            self.buses[idx].mute = !self.buses[idx].mute;
        }
        y += 40.0;

        // compact 6-band EQ curve
        let eq_rect = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, 78.0));
        draw_bus_eq_graph(
            ui,
            eq_rect,
            &mut self.buses[idx].eq,
            accent,
            Id::new(("beq_graph", idx)),
            enabled,
        );
        y += 86.0;
        // FUN_00537110 reserves a 20x13 px box above each master fader for the
        // numeric meter readout (`%0.0f` of 20*log10, or "-" at or below the
        // -96.0 dB threshold 0x56de00). Keep that slot here.
        let readout_y = y;
        y += 11.0;

        // fader and meter
        let bottom = inner.bottom() - 15.0;
        let fh = (bottom - y).max(40.0);
        let area = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(w, fh));
        let fader_w = 40.0;
        let pair_w = fader_w + 3.0 + 9.0;
        let fx = area.center().x - pair_w * 0.5;
        let fader_rect = Rect::from_min_size(Pos2::new(fx, area.top()), Vec2::new(fader_w, fh));
        let meter_rect = Rect::from_min_size(
            Pos2::new(fader_rect.right() + 4.0, area.top()),
            Vec2::new(9.0, fh),
        );
        let mut gain = self.buses[idx].gain_db;
        let mut limit = GAIN_MAX;
        let meter = self.buses[idx].meter;
        let peak = self.buses[idx].peak;
        let max_db = if self.limit_remote { 0.0 } else { GAIN_MAX };
        fader(
            ui,
            fader_rect,
            &mut gain,
            meter,
            peak,
            &mut limit,
            accent,
            Id::new(("bfader", idx)),
            enabled,
            false,
            max_db,
            self.streamer_brightness(self.active_strips + idx),
            "Bus gain: drag, wheel steps 3 dB, double click for 0 dB",
        );
        meter_widget(
            ui,
            meter_rect,
            meter,
            peak,
            &mut limit,
            Id::new(("bmeter", idx)),
            false,
            BUS_METER_MIN_DB,
            BUS_METER_SPAN_DB,
            BUS_FADER_PAD,
        );
        let meter_db = lin_to_db(meter).max(METER_FLOOR_DB);
        let readout = if meter_db <= METER_READOUT_DB {
            "-".to_string()
        } else {
            format!("{meter_db:.0}")
        };
        ui.painter().text(
            Pos2::new(meter_rect.center().x, readout_y),
            Align2::CENTER_TOP,
            readout,
            FontId::proportional(7.0),
            if meter_db > 0.0 { RED } else { DIM },
        );
        self.buses[idx].gain_db = gain.clamp(GAIN_MIN, max_db);
        ui.painter().text(
            Pos2::new(area.center().x, area.bottom() + 2.0),
            Align2::CENTER_TOP,
            fmt_db(self.buses[idx].gain_db),
            FontId::proportional(7.5),
            if self.buses[idx].mute { FAINT } else { TEXT },
        );
    }

    // -----------------------------------------------------------------------
    // Recorder / status bar
    // -----------------------------------------------------------------------

    fn draw_bottom(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let p = ui.painter();
        p.rect_filled(rect, 0.0, Color32::from_rgb(18, 21, 26));
        p.line_segment(
            [rect.left_top(), rect.right_top()],
            Stroke::new(1.0, Color32::from_rgb(45, 52, 63)),
        );
        let inner = rect.shrink2(Vec2::new(10.0, 5.0));
        let enabled = !self.lock_gui;

        // first row
        let row1_y = inner.top();
        p.text(
            Pos2::new(inner.left(), row1_y + 2.0),
            Align2::LEFT_TOP,
            "TAPE",
            FontId::proportional(9.5),
            AMBER,
        );
        p.text(
            Pos2::new(inner.left(), row1_y + 12.0),
            Align2::LEFT_TOP,
            "RECORDER",
            FontId::proportional(6.5),
            FAINT,
        );

        // file selector
        let file_rect = Rect::from_min_size(Pos2::new(inner.left() + 56.0, row1_y), Vec2::new(190.0, 22.0));
        let file_tip = if let Some(err) = &self.tape_error {
            format!("{err}\n\nClick to Select Audio file for Playback")
        } else if let Some(info) = &self.tape_info {
            format!("{info}\n\nClick to Select Audio file for Playback")
        } else {
            "Click to Select Audio file for Playback".to_string()
        };
        let file_hover = ui
            .interact(file_rect, Id::new("tape_file"), Sense::click())
            .on_hover_text(file_tip);
        p.rect_filled(file_rect, 3.0, PANEL_HI);
        p.rect_stroke(
            file_rect,
            3.0,
            Stroke::new(
                1.0,
                if self.tape_error.is_some() { RED } else { LINE },
            ),
        );
        p.text(
            file_rect.center(),
            Align2::CENTER_CENTER,
            fit_text(p, &self.tape.file, 8.0, file_rect.width() - 8.0),
            FontId::proportional(8.0),
            if self.tape_error.is_some() {
                Color32::from_rgb(238, 140, 128)
            } else {
                TEXT
            },
        );
        if file_hover.clicked() && enabled {
            self.tape_picker = true;
        }

        // transport time
        let time_txt = format!(
            "{} / {}",
            clock(self.tape.pos),
            if self.tape.recording {
                clock(self.tape.pos)
            } else {
                clock(self.tape.len)
            }
        );
        p.text(
            Pos2::new(inner.left() + 254.0, row1_y + 6.0),
            Align2::LEFT_TOP,
            time_txt,
            FontId::proportional(8.0),
            DIM,
        );

        // transport buttons
        let mut bx = inner.left() + 320.0;
        let by = row1_y + 1.0;
        let mut buttons: Vec<(Rect, Transport)> = Vec::new();
        for t in [
            Transport::Rew,
            Transport::Ff,
            Transport::Play,
            Transport::Stop,
            Transport::Rec,
        ] {
            let w = if t == Transport::Rec { 34.0 } else { 26.0 };
            let r = Rect::from_min_size(Pos2::new(bx, by), Vec2::new(w, 20.0));
            buttons.push((r, t));
            bx += w + 3.0;
        }
        for (r, t) in buttons {
            let active = match t {
                Transport::Play => self.tape.playing,
                Transport::Rec => self.tape.recording,
                _ => false,
            };
            let col = if t == Transport::Rec {
                RED
            } else if t == Transport::Play {
                GREEN
            } else {
                BLUE
            };
            if transport_button(ui, r, t, active, col, enabled) {
                self.on_transport(t);
            }
        }

        // tape routing
        let mut rx = inner.left() + 495.0;
        for i in 0..audio::MAX_BUSES {
            let r = Rect::from_min_size(Pos2::new(rx, by), Vec2::new(22.0, 20.0));
            let label = format!("{}{}", if i < 3 { "A" } else { "B" }, if i < 3 { i + 1 } else { i - 2 });
            let route = self.tape.route[i];
            let (clicked, _) = chip(
                ui,
                r,
                &label,
                route,
                if i < 3 { AMBER } else { BLUE },
                enabled,
                Id::new(("tape_route", i)),
                "Route the recorder playback into this bus",
            );
            if clicked {
                self.tape.route[i] = !route;
            }
            rx += 24.0;
        }

        // tape gain
        let gain_rect = Rect::from_min_size(Pos2::new(inner.left() + 644.0, by), Vec2::new(108.0, 20.0));
        let gain_resp = ui.interact(gain_rect, Id::new("tape_gain"), Sense::click_and_drag());
        p.rect_filled(gain_rect, 3.0, TRACK);
        p.rect_stroke(gain_rect, 3.0, Stroke::new(1.0, LINE));
        let t = ((self.tape.gain_db - GAIN_MIN) / (GAIN_MAX - GAIN_MIN)).clamp(0.0, 1.0);
        let fill = Rect::from_min_size(
            gain_rect.left_top(),
            Vec2::new(gain_rect.width() * t, gain_rect.height()),
        );
        p.rect_filled(fill, 3.0, Color32::from_rgb(50, 90, 70));
        p.text(
            gain_rect.center(),
            Align2::CENTER_CENTER,
            format!("PLAY {}", fmt_db(self.tape.gain_db)),
            FontId::proportional(7.0),
            TEXT,
        );
        if gain_resp.dragged() && enabled {
            let dx = ui.input(|i| i.pointer.delta().x);
            self.tape.gain_db =
                (self.tape.gain_db + dx * (GAIN_MAX - GAIN_MIN) / gain_rect.width()).clamp(GAIN_MIN, GAIN_MAX);
        }
        if gain_resp.double_clicked() && enabled {
            self.tape.gain_db = 0.0;
        }

        // right-hand status
        let status_color = FAINT;
        let status = if self.time < self.status_until && !self.status.is_empty() {
            self.status.clone()
        } else if self.audio.available {
            format!("A1 output: {}", self.audio.device)
        } else {
            "No audio output device".to_string()
        };
        p.text(
            Pos2::new(inner.right(), row1_y + 6.0),
            Align2::RIGHT_TOP,
            status,
            FontId::proportional(7.5),
            status_color,
        );

        // second row: progress + engine status
        let row2 = rect.bottom() - 24.0;
        let bar = Rect::from_min_size(Pos2::new(inner.left(), row2), Vec2::new(inner.width(), 14.0));
        p.rect_filled(bar, 3.0, TRACK);
        let frac = if self.tape.recording {
            (self.tape.pos / 600.0).min(1.0)
        } else if self.tape.len > 0.0 {
            (self.tape.pos / self.tape.len).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let progress = Rect::from_min_size(
            bar.left_top(),
            Vec2::new(bar.width() * frac, bar.height()),
        );
        p.rect_filled(
            progress,
            3.0,
            if self.tape.recording {
                RED.gamma_multiply(0.7)
            } else {
                Color32::from_rgb(58, 120, 90)
            },
        );
        for i in 0..40 {
            let x = bar.left() + bar.width() * (i as f32 / 40.0);
            p.line_segment(
                [Pos2::new(x, bar.bottom() - 3.0), Pos2::new(x, bar.bottom())],
                Stroke::new(1.0, FAINT.gamma_multiply(0.7)),
            );
        }
        // FUN_0053e7b0 case 0xb4: a click inside the recorder display scrubs
        // the playhead. The display keeps an 11 px inset on each side
        // (DAT_0057cd90 + 0xb, DAT_0057cd98 - 0xb) and the position is
        // MulDiv(x - (rect.x + 11), length, (rect.w - 22)).
        let scrub = ui.interact(bar, Id::new("tape_scrub"), Sense::click_and_drag());
        if enabled && self.tape.len > 0.0 && !self.tape.recording {
            if let Some(pos) = scrub.interact_pointer_pos() {
                if scrub.clicked() || scrub.dragged() {
                    let t = ((pos.x - bar.left() - 11.0) / (bar.width() - 22.0).max(1.0))
                        .clamp(0.0, 1.0);
                    let seek = t * self.tape.len;
                    self.tape.pos = seek;
                    self.audio.seek(seek);
                }
            }
        }
        let (latency_ms, latency_detail) = self.latency_report();
        let engine_text = format!(
            "{} kHz  |  buffer {}  |  engine {}  |  {}  |  latency ~{:.0} ms",
            self.sample_rate as f32 / 1000.0,
            self.buffer,
            if self.engine_mode == 0 { "Normal" } else { "Swift" },
            if self.tape.recording {
                "RECORDING"
            } else if self.tape.playing {
                "PLAYING"
            } else {
                "STOPPED"
            },
            latency_ms
        );
        let tw = p
            .layout_no_wrap(engine_text.clone(), FontId::proportional(7.0), DIM)
            .size()
            .x;
        let backdrop = Rect::from_min_max(
            Pos2::new(bar.right() - tw - 10.0, bar.top() + 1.0),
            Pos2::new(bar.right() - 2.0, bar.bottom() - 1.0),
        );
        p.rect_filled(backdrop, 2.0, Color32::from_rgb(18, 21, 26));
        p.text(
            Pos2::new(bar.right() - 6.0, bar.center().y),
            Align2::RIGHT_CENTER,
            engine_text,
            FontId::proportional(7.0),
            DIM,
        );
    }

    fn on_transport(&mut self, t: Transport) {
        if self.lock_gui {
            self.toast("Interface is locked - unlock it in the menu");
            return;
        }
        match t {
            Transport::Rew => {
                self.tape.pos = (self.tape.pos - 10.0).max(0.0);
                self.audio.seek(self.tape.pos);
                self.toast("Recorder rewind");
            }
            Transport::Ff => {
                self.tape.pos = (self.tape.pos + 10.0).min(self.tape.len);
                self.audio.seek(self.tape.pos);
                self.toast("Recorder fast forward");
            }
            Transport::Play => {
                if self.tape.file.starts_with("No file") {
                    self.toast("Choose an audio file first");
                } else if !self.audio.available {
                    self.toast("No audio output device - check Windows sound output");
                } else {
                    if !self.audio.has_source() {
                        self.reload_tape_audio();
                    }
                    if !self.audio.has_source() {
                        self.toast("The audio file is no longer available");
                    } else if self.tape.playing {
                        self.tape.playing = false;
                        self.audio.pause();
                        self.toast("Tape paused");
                    } else {
                        if self.tape.pos >= self.tape.len - 0.01 {
                            self.tape.pos = 0.0;
                            self.audio.seek(0.0);
                        }
                        // FUN_0053e7b0 case 0x12f: before starting, the
                        // playhead is pulled back to the start of its second:
                        // position_ms = (position_ms / 1000) * 1000.
                        let snapped =
                            (self.tape.pos / TAPE_POS_GRID_S).floor() * TAPE_POS_GRID_S;
                        if (snapped - self.tape.pos).abs() > 0.001 {
                            self.tape.pos = snapped;
                            self.audio.seek(snapped);
                        }
                        self.tape.playing = true;
                        self.tape.recording = false;
                        self.audio.play();
                        self.toast(format!("Playing {}", self.tape.file));
                    }
                }
            }
            Transport::Stop => {
                // FUN_0053e7b0 case 0x131 stops through FUN_004e6c80 with the
                // current position, so the transport stays where it is and a
                // later PLAY resumes there (after the whole-second snap).
                let pos = self.tape.pos;
                self.audio.pause();
                self.audio.seek(pos);
                self.tape.playing = false;
                self.tape.recording = false;
                self.tape.pos = pos;
                self.toast("Recorder stop");
            }
            Transport::Rec => {
                // Recording in the original writes through the virtual audio
                // driver. This rebuild has no recorder engine, so REC keeps its
                // original look and counter but does not create a file.
                self.audio.stop();
                self.tape.playing = false;
                if self.tape.recording {
                    self.tape.recording = false;
                } else {
                    self.tape.recording = true;
                    self.tape.pos = 0.0;
                    self.toast("Recording is not available in this rebuild");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Menu popup
    // -----------------------------------------------------------------------

    /// Bottom-right resize grip. The window is undecorated, so the OS has no
    /// border to grab; this asks winit to start an interactive resize instead.
    fn draw_resize_grip(&mut self, ctx: &egui::Context) {
        let corner = ctx.screen_rect().right_bottom();
        let size = 18.0;
        egui::Area::new(egui::Id::new("resize_grip"))
            .fixed_pos(egui::pos2(corner.x - size, corner.y - size))
            .order(egui::Order::Tooltip)
            .interactable(true)
            .show(ctx, |ui| {
                // Hover sense on purpose: egui drag state is not involved, so
                // the pointer cannot stay captured once the OS takes over the
                // resize (which used to block the title-bar drag afterwards).
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
                if response.contains_pointer() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                }
                if response.contains_pointer() && ui.input(|i| i.pointer.primary_pressed()) {
                    let maximized =
                        ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
                    if maximized {
                        // A maximized window sits on the screen corner, where
                        // the OS ignores a resize request: restore first so the
                        // grip always does something useful.
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Maximized(false));
                    } else {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                                egui::ResizeDirection::SouthEast,
                            ));
                    }
                    // The OS takes the pointer for the resize and egui never sees
                    // the release. Clear the pointer state so a later press on
                    // the title bar starts a normal drag instead of being
                    // swallowed by a drag that never ended.
                    ui.ctx().input_mut(|i| {
                        i.pointer = egui::PointerState::default();
                    });
                }
                let painter = ui.painter();
                for i in 0..3 {
                    let offset = 5.0 + i as f32 * 4.0;
                    painter.line_segment(
                        [
                            Pos2::new(rect.right() - offset, rect.bottom() - 3.0),
                            Pos2::new(rect.right() - 3.0, rect.bottom() - offset),
                        ],
                        Stroke::new(1.0, FAINT),
                    );
                }
            });
    }

    fn draw_menu(&mut self, ctx: &egui::Context) {
        if !self.menu_open {
            return;
        }
        let mut close = false;
        let area_resp = egui::Area::new(Id::new("main_menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(Pos2::new(8.0, 38.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_rgb(28, 32, 40))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(62, 70, 84)))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::same(5.0))
                    .show(ui, |ui| {
                        ui.set_width(620.0);
                        ui.columns(2, |cols| {
                            let (left_slice, right_slice) = cols.split_at_mut(1);
                            let left = &mut left_slice[0];
                            let right = &mut right_slice[0];
                        if menu_item(left, "Show Control Panel...", "", true) {
                            self.show_system = true;
                            close = true;
                        }
                        if menu_item(left, "Audio Stats...", "", true) {
                            self.show_audio_stats = true;
                            close = true;
                        }
                        if menu_item(left, "Ducking...", "", true) {
                            self.show_ducking = true;
                            close = true;
                        }
                        if menu_item(left, "Restart Audio Engine", "Ctrl+R", true) {
                            self.menu_command(CMD_RESTART_ENGINE);
                            close = true;
                        }
                        if check_item(
                            left,
                            "Auto Restart Audio Engine (A1 Device)",
                            self.auto_restart_a1,
                        ) {
                            self.menu_command(CMD_AUTO_RESTART_A1);
                        }
                        if check_item(
                            left,
                            "Auto Restart Audio Engine (All Device)",
                            self.auto_restart_all,
                        ) {
                            self.menu_command(CMD_AUTO_RESTART_ALL);
                        }
                        left.separator();
                        if menu_item(left, "Load Settings...", "", true) {
                            self.load_settings(true);
                            close = true;
                        }
                        if check_item(left, "Load Settings on Startup", self.load_on_startup) {
                            self.load_on_startup = !self.load_on_startup;
                            self.toast(if self.load_on_startup {
                                "Settings will load when the console starts"
                            } else {
                                "The console will start with the default scene"
                            });
                        }
                        if menu_item(left, "Save Settings...", "", true) {
                            self.save_settings();
                            close = true;
                        }
                        if check_item(left, "Save Settings on Exit", self.save_on_exit) {
                            self.save_on_exit = !self.save_on_exit;
                            self.toast(if self.save_on_exit {
                                "Settings will be saved when the console closes"
                            } else {
                                "Settings are only saved with Save Settings"
                            });
                        }
                        if menu_item(left, "Reset Settings (Re-Initialization)", "", true) {
                            self.reset_all();
                            close = true;
                        }
                        left.separator();
                        if menu_item(left, "Preset Scene...", "Ctrl+4", true) {
                            self.open_presets();
                            close = true;
                        }
                        if check_item(
                            left,
                            "Show Preset Scene On Startup",
                            self.show_preset_on_startup,
                        ) {
                            self.show_preset_on_startup = !self.show_preset_on_startup;
                            self.toast(if self.show_preset_on_startup {
                                "Preset scenes will open when the console starts"
                            } else {
                                "The console will start on the console view"
                            });
                        }
                        if menu_item(left, "System Settings...", "Ctrl+',''", true) {
                            self.show_system = true;
                            close = true;
                        }
                        if menu_item(left, "Virtual Cables...", "Ctrl+Shift+V", true) {
                            self.show_vc = true;
                            close = true;
                        }
                        if menu_item(left, "Tape Recorder Options...", "Ctrl+5", true) {
                            self.show_tape_opts = true;
                            close = true;
                        }
                        if menu_item(left, "Eject Cassette (release audio file)", "", true) {
                            self.eject_tape();
                            close = true;
                        }
                        if menu_item(right, "M.I.D.I. Mapping...", "Ctrl+3", true) {
                            self.show_info = Some((
                                "M.I.D.I. Mapping".to_string(),
                                "This rebuild is a control surface and does not open MIDI ports."
                                    .to_string(),
                            ));
                            close = true;
                        }
                        if menu_item(right, "VBAN Options...", "Ctrl+1", true) {
                            self.show_vban = true;
                            close = true;
                        }
                        if menu_item(right, "Check Driver Installation...", "", true) {
                            self.show_info = Some((
                                "Check Driver Installation".to_string(),
                                "The virtual audio inputs and outputs are Windows drivers installed \
                                 alongside the original."
                                    .to_string(),
                            ));
                            close = true;
                        }
                        right.separator();
                        if check_item(right, "Lock Graphic User Interface", self.lock_gui) {
                            self.lock_gui = !self.lock_gui;
                        }
                        if check_item(right, "Limit Remote Gain to 0 dBfs", self.limit_remote) {
                            self.limit_remote = !self.limit_remote;
                        }
                        if check_item(right, "Show Contextual Help In Caption", self.help_caption) {
                            self.help_caption = !self.help_caption;
                        }
                        if check_item(right, "Monitor on BUS SEL", self.monitor_on_sel) {
                            self.monitor_on_sel = !self.monitor_on_sel;
                        }
                        right.separator();
                        if menu_item(right, "About Box / License...", "", true) {
                            self.show_about = true;
                            close = true;
                        }
                        if menu_item(right, "Online Help / Support...", "", true) {
                            self.show_info = Some((
                                "Online Help / Support".to_string(),
                                "."
                                    .to_string(),
                            ));
                            close = true;
                        }
                        if menu_item(right, "Exit Menu", "Esc", true) {
                            close = true;
                        }
                        right.separator();
                        if menu_item(right, "Shut Down Audio Mixer PRO", "", true) {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        }); // columns
                    });
            });
        let clicked_outside = ctx.input(|i| i.pointer.any_click())
            && !area_resp
                .response
                .rect
                .contains(ctx.input(|i| i.pointer.interact_pos()).unwrap_or(Pos2::ZERO));
        if self.menu_ignore_click {
            self.menu_ignore_click = false;
        } else if close || clicked_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.menu_open = false;
        }
    }

    // -----------------------------------------------------------------------
    // Dialogs
    // -----------------------------------------------------------------------

    fn draw_dialogs(&mut self, ctx: &egui::Context) {
        if self.show_vc {
            let mut open = self.show_vc;
            self.vc.draw(ctx, &mut open, self.driver_mode);
            self.show_vc = open;
        }
        if self.show_system {
            let mut open = self.show_system;
            egui::Window::new("System Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(360.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Engine mode:");
                        ui.selectable_value(&mut self.engine_mode, 0, "Normal");
                        ui.selectable_value(&mut self.engine_mode, 1, "Swift");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Sample rate:");
                        egui::ComboBox::from_id_source("sr")
                            .selected_text(format!("{} Hz", self.sample_rate))
                            .show_ui(ui, |ui| {
                                // Exactly the rates StartAudioStream (FUN_00527ad0) accepts.
                                for sr in SAMPLE_RATES {
                                    ui.selectable_value(&mut self.sample_rate, sr, format!("{sr} Hz"));
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Main output buffer:");
                        egui::ComboBox::from_id_source("mainbuf")
                            .selected_text(format!("{} samples", self.main_buffer))
                            .show_ui(ui, |ui| {
                                // FUN_00527ad0 refuses anything above 2048 samples.
                                for b in [256u32, 512, 1024, 2048] {
                                    ui.selectable_value(
                                        &mut self.main_buffer,
                                        b,
                                        format!("{b} samples"),
                                    );
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Latency profile:");
                        for (label, samples) in
                            [("Low", 256u32), ("Balanced", 512), ("Safe", 1024)]
                        {
                            if ui
                                .selectable_label(self.main_buffer == samples, label)
                                .clicked()
                            {
                                self.main_buffer = samples;
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "~{:.1} ms buffer (click Apply)",
                                self.main_buffer as f32
                                    / self.sample_rate.max(1) as f32
                                    * 1000.0
                            ))
                            .size(9.0)
                            .color(FAINT),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("VAIO buffer:");
                        egui::ComboBox::from_id_source("buf")
                            .selected_text(format!("{} samples", self.buffer))
                            .show_ui(ui, |ui| {
                                for b in [512u32, 1024, 2048, 4096, 7168, 8192] {
                                    let label = if b == VAIO_BUFFER_DEFAULT {
                                        "7168 samples (recommended)".to_string()
                                    } else {
                                        format!("{b} samples")
                                    };
                                    ui.selectable_value(&mut self.buffer, b, label);
                                }
                            });
                    });
                    ui.separator();
                    // FUN_0052ab70's <OptionDev>: MME/WDM/KS 128..=2048,
                    // ASIO 0 or 64..=1024, and msA1..msA3 0..=500 ms.
                    egui::Grid::new("device_buffers")
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label("MME buffer:");
                            ui.add(
                                egui::DragValue::new(&mut self.dev_mme)
                                    .range(DEV_BUFFER_MIN..=DEV_BUFFER_MAX)
                                    .suffix(" samples"),
                            );
                            ui.end_row();
                            ui.label("WDM buffer:");
                            ui.add(
                                egui::DragValue::new(&mut self.dev_wdm)
                                    .range(DEV_BUFFER_MIN..=DEV_BUFFER_MAX)
                                    .suffix(" samples"),
                            );
                            ui.end_row();
                            ui.label("KS buffer:");
                            ui.add(
                                egui::DragValue::new(&mut self.dev_ks)
                                    .range(DEV_BUFFER_MIN..=DEV_BUFFER_MAX)
                                    .suffix(" samples"),
                            );
                            ui.end_row();
                            ui.label("ASIO buffer:");
                            ui.add(
                                egui::DragValue::new(&mut self.dev_asio)
                                    .range(0..=ASIO_BUFFER_MAX)
                                    .suffix(" samples"),
                            );
                            // FUN_0052ab70 accepts 0 or 64..=1024, not 1..63.
                            if self.dev_asio != 0 && self.dev_asio < 64 {
                                self.dev_asio = 64;
                            }
                            ui.end_row();
                            ui.label("Output latency:");
                            ui.horizontal(|ui| {
                                for (n, label) in ["A1", "A2", "A3"].iter().enumerate() {
                                    ui.label(*label);
                                    ui.add(
                                        egui::DragValue::new(&mut self.bus_latency_ms[n])
                                            .range(0.0..=DEV_MS_MAX)
                                            .speed(0.5)
                                            .suffix(" ms"),
                                    );
                                }
                            });
                            ui.end_row();
                        });
                    ui.checkbox(&mut self.dev_srasio, "Use preferred ASIO sample rate");
                    ui.separator();
                    ui.checkbox(&mut self.limit_remote, "Limit Remote Gain to 0 dBfs");
                    ui.checkbox(&mut self.monitor_on_sel, "Monitor on BUS SEL");
                    ui.checkbox(&mut self.help_caption, "Show contextual help in caption");
                    ui.add_space(4.0);
                    let patch = egui::RichText::new(
                        "Input patch: hardware 1-3 and virtual 1-2 feed strips 1-5. \
                         BUS A1-A3 are physical outputs, BUS B1-B2 are virtual outputs.",
                    )
                    .size(10.0)
                    .color(DIM);
                    ui.label(patch);
                    ui.add_space(6.0);
                    if ui.button("Apply").clicked() {
                        match self.apply_audio_format() {
                            Ok(()) => {
                                self.toast(format!(
                                    "System settings applied: {} Hz / main {} / VAIO {} samples",
                                    self.audio.sample_rate, self.main_buffer, self.buffer
                                ));
                                self.reconfigure_vst();
                                self.show_system = false;
                            }
                            Err(err) => {
                                self.show_info =
                                    Some(("A1 Output Device".to_string(), err));
                            }
                        }
                    }
                });
            if !open {
                self.show_system = false;
            }
        }

        if self.show_vban {
            let mut open = self.show_vban;
            egui::Window::new("VBAN Network")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(720.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    let status = self.vban.status();
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.vban_enabled, "Enabled");
                        ui.label("UDP port:");
                        ui.add(egui::DragValue::new(&mut self.vban_port).range(1024..=65535));
                        ui.label("Quality:");
                        egui::ComboBox::from_id_source("vban_q")
                            .selected_text(
                                ["Fast", "Medium", "Safe", "Slow", "Very slow"]
                                    [self.vban_quality.min(4) as usize],
                            )
                            .show_ui(ui, |ui| {
                                for (i, label) in
                                    ["Fast", "Medium", "Safe", "Slow", "Very slow"].iter().enumerate()
                                {
                                    ui.selectable_value(
                                        &mut self.vban_quality,
                                        i as u8,
                                        *label,
                                    );
                                }
                            });
                        if ui.button("Apply").clicked() {
                            self.apply_vban_config();
                            self.toast("VBAN settings applied");
                        }
                    });
                    let state = if status.running {
                        format!(
                            "Listening on UDP {} - {} stream(s), {} binding(s)",
                            status.port, status.streams, status.bindings
                        )
                    } else {
                        "VBAN receive is stopped".to_string()
                    };
                    ui.label(
                        egui::RichText::new(state)
                            .color(if status.running { GREEN } else { DIM }),
                    );
                    if let Some(err) = &status.error {
                        ui.colored_label(RED, err);
                    }
                    ui.label(
                        egui::RichText::new(
                            "Allow UDP through Windows Firewall. A stream appears only while it \
                             is sending to this machine's port.",
                        )
                        .size(9.0)
                        .color(FAINT),
                    );

                    ui.separator();
                    ui.label(
                        egui::RichText::new("DISCOVERED STREAMS").strong().color(AMBER),
                    );
                    let discovered = self.vban.discovered();
                    if discovered.is_empty() {
                        ui.label(
                            egui::RichText::new("Waiting for VBAN audio packets...")
                                .size(10.0)
                                .color(DIM),
                        );
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Assign to:");
                            let labels: Vec<String> = (0..self.active_strips)
                                .map(|i| self.strip_label(i))
                                .collect();
                            let selected = self
                                .vban_target_strip
                                .min(self.active_strips.saturating_sub(1));
                            egui::ComboBox::from_id_source("vban_target")
                                .selected_text(
                                    labels.get(selected).cloned().unwrap_or_default(),
                                )
                                .show_ui(ui, |ui| {
                                    for (i, label) in labels.iter().enumerate() {
                                        ui.selectable_value(
                                            &mut self.vban_target_strip,
                                            i,
                                            label,
                                        );
                                    }
                                });
                        });
                        let mut assign = None;
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for stream in &discovered {
                                    ui.horizontal(|ui| {
                                        ui.label(format!(
                                            "{}  {}:{}  {} Hz / {} ch / {}",
                                            stream.name,
                                            stream.ip,
                                            stream.port,
                                            stream.sample_rate,
                                            stream.channels,
                                            stream.format.label()
                                        ));
                                        if ui.small_button("Use").clicked() {
                                            assign = Some(stream.clone());
                                        }
                                    });
                                }
                            });
                        if let Some(stream) = assign {
                            let target = self
                                .vban_target_strip
                                .min(self.active_strips.saturating_sub(1));
                            self.captures[target] = None;
                            self.audio.detach_input(target);
                            self.set_vban_input(
                                target,
                                &stream.name,
                                &stream.ip,
                                stream.port,
                                self.vban_quality,
                            );
                            self.toast(format!(
                                "VBAN {} assigned to {}",
                                stream.name,
                                self.strip_label(target)
                            ));
                        }
                    }

                    ui.separator();
                    ui.label(
                        egui::RichText::new("INCOMING BINDINGS").strong().color(AMBER),
                    );
                    let bindings = self.vban.bindings();
                    if bindings.is_empty() {
                        ui.label(
                            egui::RichText::new("No VBAN input bindings yet.")
                                .size(10.0)
                                .color(DIM),
                        );
                    }
                    let mut unbind = None;
                    for binding in &bindings {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{}  <-  {} @ {}{}  ({} pkts, {} lost, {} dup)",
                                self.strip_label(binding.strip),
                                binding.name,
                                binding.ip,
                                if binding.waiting { " (waiting)" } else { "" },
                                binding.packets,
                                binding.lost,
                                binding.duplicates
                            ));
                            if ui.small_button("Remove").clicked() {
                                unbind = Some(binding.strip);
                            }
                        });
                    }
                    if let Some(strip) = unbind {
                        self.vban.unbind(strip);
                        self.captures[strip] = None;
                        self.audio.detach_input(strip);
                        self.strips[strip].device = "No input".to_string();
                        self.strips[strip].route = [false; audio::MAX_BUSES];
                    }

                    ui.separator();
                    ui.label(
                        egui::RichText::new("OUTGOING STREAMS").strong().color(AMBER),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Sends a bus (A1..B3) or a single input strip (CH1..) to another machine. Allow UDP to the target port.",
                        )
                        .size(9.0)
                        .color(FAINT),
                    );
                    let bus_labels = ["A1", "A2", "A3", "B1", "B2", "B3"];
                    let strip_names: Vec<String> = (0..self.active_strips)
                        .map(|i| format!("CH{}", i + 1))
                        .collect();
                    let mut remove = None;
                    for (index, out) in self.vban_out.iter_mut().enumerate() {
                        let current = match out.strip {
                            Some(strip) => strip_names
                                .get(strip)
                                .cloned()
                                .unwrap_or_else(|| format!("CH{}", strip + 1)),
                            None => bus_labels
                                .get(out.bus)
                                .copied()
                                .unwrap_or("A1")
                                .to_string(),
                        };
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_source(("vban_src", index))
                                .width(56.0)
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for (i, label) in bus_labels.iter().enumerate() {
                                        let selected = out.strip.is_none() && out.bus == i;
                                        if ui.selectable_label(selected, *label).clicked() {
                                            out.bus = i;
                                            out.strip = None;
                                        }
                                    }
                                    ui.separator();
                                    for (i, label) in strip_names.iter().enumerate() {
                                        let selected = out.strip == Some(i);
                                        if ui.selectable_label(selected, label).clicked() {
                                            out.strip = Some(i);
                                        }
                                    }
                                });
                            ui.add(
                                egui::TextEdit::singleline(&mut out.name)
                                    .desired_width(150.0)
                                    .hint_text("Stream name"),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut out.target)
                                    .desired_width(180.0)
                                    .hint_text("IP:port"),
                            );
                            if ui.small_button("x").clicked() {
                                remove = Some(index);
                            }
                        });
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_source(("vban_sr", index))
                                .selected_text(format!("{} Hz", out.sample_rate))
                                .show_ui(ui, |ui| {
                                    for sr in [44_100u32, 48_000, 96_000] {
                                        ui.selectable_value(
                                            &mut out.sample_rate,
                                            sr,
                                            format!("{sr} Hz"),
                                        );
                                    }
                                });
                            egui::ComboBox::from_id_source(("vban_fmt", index))
                                .selected_text(out.format.label())
                                .show_ui(ui, |ui| {
                                    for format in [
                                        vban::VbanFormat::Pcm16,
                                        vban::VbanFormat::Pcm24,
                                        vban::VbanFormat::Float32,
                                    ] {
                                        ui.selectable_value(
                                            &mut out.format,
                                            format,
                                            format.label(),
                                        );
                                    }
                                });
                            egui::ComboBox::from_id_source(("vban_oq", index))
                                .selected_text(format!("Quality {}", out.quality))
                                .show_ui(ui, |ui| {
                                    for quality in 0..=4u8 {
                                        ui.selectable_value(
                                            &mut out.quality,
                                            quality,
                                            format!("Quality {quality}"),
                                        );
                                    }
                                });
                        });
                    }
                    if let Some(index) = remove {
                        self.vban_out.remove(index);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("+ Add outgoing").clicked() {
                            let mut out = vban::VbanOutConfig::default();
                            out.sample_rate = self.audio.sample_rate.max(8_000);
                            self.vban_out.push(out);
                        }
                        if ui.button("Apply outgoing").clicked() {
                            self.apply_vban_config();
                            self.toast("VBAN outgoing streams applied");
                        }
                    });
                    for info in self.vban.sender_info() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} -> {} [{}] {} pkts, {} err{}",
                                bus_labels.get(info.bus).copied().unwrap_or("?"),
                                info.target,
                                info.name,
                                info.packets,
                                info.errors,
                                info.last_error
                                    .map(|err| format!(": {err}"))
                                    .unwrap_or_default()
                            ))
                            .size(9.0)
                            .color(DIM),
                        );
                    }
                });
            self.show_vban = open;
        }

        if self.show_audio_stats {
            let mut open = self.show_audio_stats;
            egui::Window::new("Audio Stats")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(470.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Mixer: {} | {} Hz | {} frames | {:.1} ms ring | callback load {:.0}%",
                        self.audio.device,
                        self.audio.sample_rate,
                        self.audio.callback_frames(),
                        self.audio.output_latency_ms(),
                        self.audio.callback_load() * 100.0
                    ));
                    ui.separator();
                    ui.label(egui::RichText::new("VIRTUAL CABLES").strong().color(AMBER));
                    for n in 1..=self.vc_cfg.cables {
                        if let Some(device) = self.vc.device(n) {
                            ui.label(format!(
                                "Cable {:02}: buffer {} ms | inject {} ms (fill {:.1} ms) | capture {} Hz{}",
                                n,
                                device.buffer_ms(),
                                device.inject_ms(),
                                device.inject_fill_ms(),
                                device.capture_rate(),
                                if device.capture_active() { "" } else { " (idle)" }
                            ));
                        }
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("INPUTS").strong().color(AMBER));
                    for i in 0..self.active_strips {
                        if let Some(cap) = self.captures[i].as_ref() {
                            let mixed = self.strips[i].route[0]
                                || self.strips[i].route[3]
                                || self.strips[i].route[4];
                            ui.label(format!(
                                "{}: {} | {} Hz | ring {:.1} ms | last {} frames{}",
                                self.strips[i].name,
                                cap.device(),
                                cap.sample_rate(),
                                cap.ring_fill_ms(),
                                cap.last_frames(),
                                if mixed { "" } else { " (not mixed to A1/B1/B2)" }
                            ));
                        }
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("VST LATENCY").strong().color(AMBER));
                    for i in 0..self.active_strips {
                        if self.vsts[i].loaded() {
                            let (latency, tail) = self.vsts[i].chain.latency();
                            let name = self.vsts[i]
                                .info
                                .as_ref()
                                .map(|info| info.name.clone())
                                .unwrap_or_else(|| "plug-in".to_string());
                            ui.label(format!(
                                "{} - {}: {} samples ({:.1} ms), tail {}",
                                self.strips[i].name,
                                name,
                                latency,
                                latency as f32 / self.sample_rate.max(1) as f32 * 1000.0,
                                tail
                            ));
                        }
                    }
                });
            self.show_audio_stats = open;
        }

        if self.show_about {
            let mut open = self.show_about;
            egui::Window::new("About")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(400.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(42.0), Sense::hover());
                        ui.painter().rect_filled(r, 8.0, AMBER);
                        ui.painter().text(
                            r.center(),
                            Align2::CENTER_CENTER,
                            "AM",
                            FontId::proportional(16.0),
                            Color32::from_rgb(30, 22, 10),
                        );
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("AUDIO MIXER PRO")
                                    .size(18.0)
                                    .strong(),
                            );
                            ui.label(
                                egui::RichText::new("Virtual audio mixing console - rebuilt control surface")
                                    .size(10.0)
                                    .color(DIM),
                            );
                        });
                    });
                    ui.add_space(6.0);
                    ui.label("Version 2.1.2.2 (reconstruction)");
                    ui.label("5 inputs: 3 hardware + 2 virtual");
                    ui.label("5 buses: A1, A2, A3 physical + B1, B2 virtual");
                    ui.label("Per-strip gain, pan, 3-band EQ, compressor, gate, limiter");
                    ui.label("Per-bus gain, 6-band EQ, mono and 12 mix modes");
                    ui.label("Tape deck with play, rewind, fast forward and WAV loading");
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Engine dynamics (fixed values)")
                            .size(10.0)
                            .color(DIM),
                    );
                    ui.label(format!(
                        "Compressor: I/O {:.1}/{:.1} dB, {:.0} ms attack, {:.0} ms release, \
                         {:.2} knee, {:.3} ratio, {:.0} dB threshold, auto make-up {}",
                        COMP_GAININ_DB,
                        COMP_GAINOUT_DB,
                        COMP_ATTACK_MS,
                        COMP_RELEASE_MS,
                        COMP_KNEE,
                        COMP_RATIO,
                        COMP_THRESHOLD_DB,
                        if COMP_AUTOMAKEUP != 0 { "on" } else { "off" }
                    ));
                    ui.label(format!(
                        "Gate: {:.0} dB threshold, {:.0} dB damping, {:.0}% side-chain, \
                         {:.0} ms attack, {:.0} ms hold, {:.0} ms release",
                        GATE_THRESHOLD_DB,
                        GATE_DAMPING_DB,
                        GATE_SIDECHAIN_PCT,
                        GATE_ATTACK_MS,
                        GATE_HOLD_MS,
                        GATE_RELEASE_MS
                    ));
                    ui.label(
                        egui::RichText::new(
                            "COMP and GATE are the original's 0..10 audibility knobs; \
                             these engine values are fixed and are not saved in the scene.",
                        )
                        .size(9.0)
                        .color(DIM),
                    );
                    ui.separator();
                    if self.audio.available {
                        ui.label(format!(
                            "Audio output: {} - {} Hz, {} channels",
                            self.audio.device, self.audio.sample_rate, self.audio.channels
                        ));
                    } else {
                        ui.label(
                            egui::RichText::new(format!("No audio output: {}", self.audio.status))
                                .color(RED),
                        );
                    }
                    ui.label(
                        egui::RichText::new(
                            "The console sends the tape deck's stereo material to your default \
                             Windows output. The meters show real measured levels: hardware \
                             strips follow the selected microphone or line input, Virtual 1 \
                             follows the tape material, and BUS A1 follows the signal leaving \
                             the output device. Nothing on the meters is generated.",
                        )
                        .size(10.0)
                        .color(DIM),
                    );
                    ui.add_space(4.0);
                    if ui.button("Close").clicked() {
                        self.show_about = false;
                    }
                });
            if !open {
                self.show_about = false;
            }
        }

        if self.show_tape_opts {
            let mut open = self.show_tape_opts;
            egui::Window::new("Tape Recorder Options")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(340.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    egui::Grid::new("tape_opts")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            // FUN_0052cd90 accepts FileType 1/2/3/100,
                            // BitRes >> 3 = 1..4, only 2/4/6/8 channels,
                            // the nine TargetSR rates and the 14 kbps
                            // values 32..320; preRecTime is capped at 20 s.
                            ui.label("File type:");
                            ui.label(format!(
                                "WAV (PCM), type {} (accepted {:?})",
                                self.tape.rec_file_type, REC_FILE_TYPES
                            ));
                            ui.end_row();
                            ui.label("Bit resolution:");
                            ui.label(format!(
                                "{} bit (accepted {:?})",
                                self.tape.rec_bit_depth, REC_BIT_DEPTHS
                            ));
                            ui.end_row();
                            ui.label("Target sample rate:");
                            ui.label(format!(
                                "{} Hz ({} accepted rates, 22050..192000)",
                                self.tape.rec_target_sr,
                                REC_TARGET_RATES.len()
                            ));
                            ui.end_row();
                            ui.label("Channels:");
                            ui.label(format!(
                                "{} (accepted {:?})",
                                self.tape.rec_channels, REC_CHANNELS
                            ));
                            ui.end_row();
                            ui.label("MP3 bitrate:");
                            ui.label(format!(
                                "{} kbps ({} accepted values, 32..320)",
                                self.tape.rec_kbps,
                                REC_KBPS.len()
                            ));
                            ui.end_row();
                            ui.label("Pre-roll:");
                            ui.label(format!(
                                "{} s (0..{} s)",
                                self.tape.rec_pre_rec, REC_PRE_REC_MAX_S
                            ));
                            ui.end_row();
                            ui.label("Timer stop:");
                            ui.label(format!(
                                "{} s (0 = off, RecordMode TimerStop)",
                                self.tape.timer_stop
                            ));
                            ui.end_row();
                            ui.label("Armed:");
                            let armed_in: Vec<String> = (1..=8)
                                .filter(|n| self.tape.arm & (1 << (n - 1)) != 0)
                                .map(|n| n.to_string())
                                .collect();
                            let armed_bus: Vec<String> = (1..=8)
                                .filter(|n| self.tape.arm & (1 << (15 + n)) != 0)
                                .map(|n| n.to_string())
                                .collect();
                            ui.label(format!(
                                "inputs [{}] buses [{}] (RecordArm bits)",
                                armed_in.join(", "),
                                armed_bus.join(", ")
                            ));
                            ui.end_row();
                            ui.label("End of file:");
                            ui.checkbox(
                                &mut self.tape.loop_playback,
                                "Loop (RecordMode Loop)",
                            );
                            ui.end_row();
                        });
                    ui.separator();
                    ui.label(
                        egui::RichText::new(
                            "Recordings follow the selected bus routing. \
                             Click the file name in the tape deck to choose material.",
                        )
                        .size(10.0)
                        .color(DIM),
                    );
                    if ui.button("Close").clicked() {
                        self.show_tape_opts = false;
                    }
                });
            if !open {
                self.show_tape_opts = false;
            }
        }

        // -------------------------------------------------------------------
        // Preset Scene (FUN_0052d990 / FUN_0052de50)
        // -------------------------------------------------------------------
        if self.show_presets {
            let mut open = self.show_presets;
            let mut do_save = false;
            let mut do_load: Option<PathBuf> = None;
            let mut do_delete: Option<PathBuf> = None;
            egui::Window::new("Preset Scene")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(460.0)
                .default_height(360.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Save the current console as a named preset, or restore one. \
                             Presets keep every strip, bus and recorder value.",
                        )
                        .size(10.0)
                        .color(DIM),
                    );
                    ui.add_space(4.0);
                    egui::Grid::new("preset_save")
                        .num_columns(2)
                        .spacing([10.0, 5.0])
                        .show(ui, |ui| {
                            ui.label("Name:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.preset_name)
                                    .desired_width(280.0)
                                    .hint_text("Preset name"),
                            );
                            ui.end_row();
                            ui.label("Comment:");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.preset_comment)
                                    .desired_width(280.0)
                                    .hint_text("Optional note"),
                            );
                            ui.end_row();
                        });
                    ui.horizontal(|ui| {
                        if ui.button("Save Preset Scene").clicked() {
                            do_save = true;
                        }
                        if ui.button("Refresh List").clicked() {
                            self.preset_entries = scan_presets();
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} of 64 scene slots filled",
                                self.preset_entries.len()
                            ))
                            .size(10.0)
                            .color(DIM),
                        );
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            if self.preset_entries.is_empty() {
                                ui.label(
                                    egui::RichText::new("01 - Empty Preset")
                                        .size(10.0)
                                        .color(DIM),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "The original console shows 64 fixed scene rows. \
                                         Name one above and press Save to fill the first slot.",
                                    )
                                    .size(9.0)
                                    .color(DIM),
                                );
                            }
                            let entries = self.preset_entries.clone();
                            for (index, entry) in entries.into_iter().enumerate() {
                                ui.horizontal(|ui| {
                                    let active = entry.name == self.preset_active;
                                    let label = if active {
                                        format!("{:02} - {}  (active)", index + 1, entry.name)
                                    } else {
                                        format!("{:02} - {}", index + 1, entry.name)
                                    };
                                    ui.label(egui::RichText::new(label).strong());
                                    if !entry.stamp.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&entry.stamp)
                                                .size(9.0)
                                                .color(DIM),
                                        );
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("Delete").clicked() {
                                                do_delete = Some(entry.path.clone());
                                            }
                                            if ui.small_button("Load").clicked() {
                                                do_load = Some(entry.path.clone());
                                            }
                                        },
                                    );
                                });
                                if !entry.comment.trim().is_empty() {
                                    ui.label(
                                        egui::RichText::new(&entry.comment)
                                            .size(9.0)
                                            .color(DIM),
                                    );
                                }
                            }
                        });
                    if !self.preset_status.is_empty() {
                        ui.separator();
                        ui.label(
                            egui::RichText::new(&self.preset_status)
                                .size(10.0)
                                .color(AMBER),
                        );
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        self.show_presets = false;
                    }
                });
            if do_save {
                match self.save_preset() {
                    Ok(path) => {
                        self.preset_status = format!("Saved {}", path.display());
                        self.toast(format!("Preset scene \"{}\" saved", self.preset_active));
                    }
                    Err(e) => self.preset_status = e,
                }
            }
            if let Some(path) = do_load {
                self.load_preset(&path);
            }
            if let Some(path) = do_delete {
                match std::fs::remove_file(&path) {
                    Ok(_) => {
                        self.preset_entries = scan_presets();
                        self.preset_status = "Preset deleted".to_string();
                    }
                    Err(e) => self.preset_status = format!("Could not delete preset: {e}"),
                }
            }
            if !open {
                self.show_presets = false;
            }
        }

        // -------------------------------------------------------------------
        // VST3 inserts, one per input strip (rebuild extension)
        // -------------------------------------------------------------------
        if let Some(slot) = self.vst_panel {
            let mut open = true;
            let mut do_scan = false;
            let mut do_browse = false;
            let mut do_remove = false;
            let mut do_open_editor = false;
            let mut do_load: Option<PathBuf> = None;
            let mut new_bypass: Option<bool> = None;
            let strip_name = self.strip_label(slot);
            let info = self.vsts[slot].info.clone();
            let path_text = self.vsts[slot].path.clone();
            let bypass_flag = self.vsts[slot].bypass;
            let slot_error = self.vsts[slot].error.clone();
            egui::Window::new(format!("VST3 Plug-in - {strip_name}"))
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(580.0)
                .default_height(540.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{strip_name} insert"))
                                .size(12.0)
                                .strong()
                                .color(TEAL),
                        );
                        match &info {
                            Some(info) => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}  -  {} {}  [{}]",
                                        info.name, info.vendor, info.version, info.category
                                    ))
                                    .size(10.5)
                                    .color(TEXT),
                                );
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("No plug-in loaded")
                                        .size(10.5)
                                        .color(DIM),
                                );
                            }
                        }
                    });
                    if let Some(info) = &info {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  |  {} audio input bus(es), {} audio output bus(es)  |  {}",
                                info.path.display(),
                                info.audio_inputs,
                                info.audio_outputs,
                                if info.has_gui {
                                    "the plug-in has its own window"
                                } else {
                                    "no plug-in window"
                                }
                            ))
                            .size(9.0)
                            .color(FAINT),
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .button("Browse .vst3...")
                            .on_hover_text("Choose a single VST3 plug-in anywhere on this PC")
                            .clicked()
                        {
                            do_browse = true;
                        }
                        if ui
                            .button("Rescan standard folders")
                            .on_hover_text(
                                "Scan C:\\Program Files\\Common Files\\VST3 and the (x86) folder",
                            )
                            .clicked()
                        {
                            do_scan = true;
                        }
                        if ui
                            .add_enabled(
                                !path_text.is_empty(),
                                egui::Button::new("Remove"),
                            )
                            .clicked()
                        {
                            do_remove = true;
                        }
                    });
                    ui.add_space(2.0);
                    let can_open = info.as_ref().map_or(false, |info| info.has_gui);
                    if ui
                        .add_enabled(
                            can_open,
                            egui::Button::new("Open VST - show the plug-in's own screen")
                                .min_size(egui::Vec2::new(0.0, 26.0)),
                        )
                        .on_hover_text(
                            "Opens this channel's plug-in's own window. Plug-in settings are never drawn inside this mixer.",
                        )
                        .clicked()
                    {
                        do_open_editor = true;
                    }
                    let mut bypass = bypass_flag;
                    if ui
                        .checkbox(
                            &mut bypass,
                            "Bypass - pass this channel through untouched",
                        )
                        .changed()
                    {
                        new_bypass = Some(bypass);
                    }
                    if let Some(err) = slot_error.clone() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(err).size(10.0).color(RED));
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Installed VST3 plug-ins")
                                .size(10.5)
                                .strong(),
                        );
                        if self.vst_scanning {
                            ui.spinner();
                        }
                        ui.label(
                            egui::RichText::new(self.vst_status.clone())
                                .size(9.0)
                                .color(DIM),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Filter:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.vst_filter)
                                .desired_width(220.0)
                                .hint_text("name or vendor"),
                        );
                    });
                    egui::ScrollArea::vertical()
                        .id_salt(("vst_plugins", slot))
                        .max_height(150.0)
                        .show(ui, |ui| {
                            let needle = self.vst_filter.trim().to_ascii_lowercase();
                            let mut shown = 0usize;
                            for plugin in &self.vst_scan_results {
                                let haystack = format!(
                                    "{} {} {}",
                                    plugin.name, plugin.vendor, plugin.category
                                )
                                .to_ascii_lowercase();
                                if !needle.is_empty() && !haystack.contains(&needle) {
                                    continue;
                                }
                                shown += 1;
                                let current = info
                                    .as_ref()
                                    .map_or(false, |info| info.path == plugin.path);
                                let label = format!(
                                    "{}{}  -  {}  [{}]",
                                    if current { "* " } else { "" },
                                    plugin.name,
                                    plugin.vendor,
                                    plugin.category
                                );
                                let response = ui.selectable_label(current, label);
                                if response.clicked() {
                                    do_load = Some(plugin.path.clone());
                                }
                                let _ = response.on_hover_text(format!(
                                    "{}\n{} {}",
                                    plugin.path.display(),
                                    plugin.name,
                                    plugin.version
                                ));
                            }
                            if shown == 0 {
                                ui.label(
                                    egui::RichText::new(if self.vst_scanning {
                                        "Scanning..."
                                    } else if self.vst_scan_results.is_empty() {
                                        "No VST3 plug-in found in the standard folders. Use Browse .vst3... to pick one, or Rescan."
                                    } else {
                                        "No plug-in matches the filter."
                                    })
                                    .size(9.5)
                                    .color(DIM),
                                );
                            }
                        });
                    if !self.vst_scan_errors.is_empty() {
                        ui.collapsing(
                            format!("{} plug-in(s) could not be read", self.vst_scan_errors.len()),
                            |ui| {
                                for error in self.vst_scan_errors.iter().take(12) {
                                    ui.label(
                                        egui::RichText::new(error).size(9.0).color(AMBER),
                                    );
                                }
                            },
                        );
                    }
                    ui.separator();
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "This window only chooses the plug-in for {strip_name}. Its settings live in the plug-in's own screen - press Open VST above or beside the VST button on this channel."
                        ))
                        .size(9.0)
                        .color(DIM),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            "Every input channel has its own insert, so each one can run a different plug-in. The original console executable has no VST loader; this insert is a rebuild extension. The dry channel is passed through whenever its plug-in is bypassed, missing, or fails.",
                        )
                        .size(8.5)
                        .color(FAINT),
                    );
                });
            if !open {
                self.vst_panel = None;
            }
            if do_browse {
                if let Some(path) = vst::pick_plugin_file() {
                    self.load_vst(slot, &path, false);
                    if self.vsts[slot]
                        .info
                        .as_ref()
                        .map_or(false, |info| info.has_gui)
                    {
                        self.open_vst_editor(slot);
                    }
                }
            }
            if do_scan {
                self.start_vst_scan();
            }
            if do_remove {
                self.remove_vst(slot);
            }
            if do_open_editor {
                self.open_vst_editor(slot);
            }
            if let Some(bypass) = new_bypass {
                self.vsts[slot].bypass = bypass;
                self.vsts[slot].chain.set_bypass(bypass);
            }
            if let Some(path) = do_load {
                self.load_vst(slot, &path, false);
                if self.vsts[slot]
                    .info
                    .as_ref()
                    .map_or(false, |info| info.has_gui)
                {
                    self.open_vst_editor(slot);
                }
            }
        }

        if let Some((title, text)) = self.show_info.clone() {
            let mut open = true;
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(360.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(text).size(11.0));
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        self.show_info = None;
                    }
                });
            if !open {
                self.show_info = None;
            }
        }

        if let Some(idx) = self.bus_eq_open {
            let mut open = true;
            let name = self.buses[idx].name.clone();
            egui::Window::new(format!("BUS {name} - 6-band parametric EQ"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(470.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Master EQ");
                        ui.checkbox(&mut self.buses[idx].eq_on, "On");
                        if ui.button("Flat").clicked() {
                            for b in &mut self.buses[idx].eq {
                                b.gain = 0.0;
                            }
                        }
                    });
                    ui.separator();
                    for (i, band) in self.buses[idx].eq.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut band.on, "");
                            ui.label(format!("{}", i + 1));
                            ui.add(
                                egui::Slider::new(&mut band.freq, 20.0..=20000.0)
                                    .logarithmic(true)
                                    .suffix(" Hz")
                                    .fixed_decimals(0),
                            );
                            ui.add(
                                egui::Slider::new(&mut band.gain, -18.0..=18.0)
                                    .suffix(" dB")
                                    .fixed_decimals(1),
                            );
                            ui.add(
                                egui::Slider::new(&mut band.q, 0.1..=10.0)
                                    .prefix("Q ")
                                    .fixed_decimals(2),
                            );
                        });
                    }
                    ui.separator();
                    ui.label(
                        egui::RichText::new(
                            "Nothing to see here.",
                        )
                        .size(10.0)
                        .color(DIM),
                    );
                });
            if !open {
                self.bus_eq_open = None;
            }
        }

        if let Some(target) = self.picker {
            let mut open = true;
            let mut close_now = false;
            let title = match target {
                Picker::Strip(i) => {
                    format!("Select Input Device - {}", self.strip_label(i))
                }
                Picker::Bus(i) => format!("Select Output Device - BUS {}", self.buses[i].name),
            };
            let virtual_tail: [&str; 2] = ["Virtual 1 (VAIO)", "Virtual 2 (AUX)"];
            let is_strip_picker = matches!(target, Picker::Strip(_));
            let devices: Vec<String> = match target {
                Picker::Strip(_) => {
                    self.probe_inputs_now();
                    let mut inputs = self.last_inputs.clone();
                    if inputs.is_empty() {
                        inputs.push("No input device found".to_string());
                    }
                    let mut list =
                        vec!["\u{2014} Windows input devices (USB, virtual cables, microphones) \u{2014}".to_string()];
                    list.extend(inputs);
                    if matches!(target, Picker::Strip(i) if self.strips[i].kind == StripKind::Virtual) {
                        // The original VAIO/AUX placeholders belong to the
                        // two base virtual strips only; extra strips list real
                        // capture endpoints.
                        if matches!(target, Picker::Strip(3) | Picker::Strip(4)) {
                            list.push("\u{2014} Virtual audio inputs \u{2014}".to_string());
                            list.extend(virtual_tail.iter().map(|s| s.to_string()));
                        }
                        let vban_streams = self.vban.discovered();
                        if !vban_streams.is_empty() {
                            list.push("\u{2014} VBAN network inputs \u{2014}".to_string());
                            for stream in vban_streams {
                                list.push(format!(
                                    "VBAN: {} @ {}:{}",
                                    stream.name, stream.ip, stream.port
                                ));
                            }
                        }
                        list.push("No device".to_string());
                    } else {
                        list.push("No input".to_string());
                    }
                    list
                }
                Picker::Bus(i) if i == 0 => {
                    // A1 is the card whose output is actually opened, so list
                    // the Windows devices instead of the simulated names. The
                    // list comes from the probe thread; enumerating here would
                    // freeze the console while a cable endpoint is wedged.
                    self.probe_devices_now();
                    let mut outputs = self.last_outputs.clone();
                    if outputs.is_empty() {
                        outputs.push("No audio output device".to_string());
                    }
                    outputs
                }
                Picker::Bus(_) => {
                    self.probe_devices_now();
                    let mut outputs = vec!["No device".to_string()];
                    outputs.extend(self.last_outputs.iter().cloned());
                    if outputs.len() == 1 {
                        outputs.push("No audio output device".to_string());
                    }
                    outputs
                }
            };
            let current = match target {
                Picker::Strip(i) => self.strips[i].device.clone(),
                Picker::Bus(i) => self.buses[i].device.clone(),
            };
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(300.0)
                .max_height(520.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(430.0)
                        .show(ui, |ui| {
                    if is_strip_picker {
                        let found = devices
                            .iter()
                            .filter(|d| {
                                !d.starts_with('\u{2014}')
                                    && d.as_str() != "No input"
                                    && d.as_str() != "No device"
                                    && d.as_str() != "No input device found"
                            })
                            .count();
                        ui.label(
                            egui::RichText::new(format!(
                                "{found} input device(s) found on this PC"
                            ))
                            .size(10.0)
                            .color(DIM),
                        );
                    }
                    for d in devices {
                        if d.starts_with('\u{2014}') {
                            ui.add_space(5.0);
                            ui.label(
                                egui::RichText::new(d.trim_matches('\u{2014}').trim())
                                    .size(9.0)
                                    .color(FAINT),
                            );
                            continue;
                        }
                        if ui.selectable_label(d == current, d.as_str()).clicked() {
                            match target {
                                Picker::Strip(i) => {
                                    self.pending_auto_assign.retain(|&x| x != i);
                                    if d == "No input"
                                        || d == "No input device found"
                                        || d == "No device"
                                    {
                                        self.vban.unbind(i);
                                        self.captures[i] = None;
                                        self.audio.detach_input(i);
                                        self.strips[i].device = "No input".to_string();
                                        self.strips[i].meter = 0.0;
                                        self.strips[i].peak = 0.0;
                                        self.strips[i].route = [false; audio::MAX_BUSES];
                                        self.strips[i].tape_feed = false;
                                        self.toast("Input closed - meter stays at silence");
                                    } else if i >= 3
                                        && self.strips[i].kind == StripKind::Virtual
                                        && (d == virtual_tail[0] || d == virtual_tail[1])
                                    {
                                        self.vban.unbind(i);
                                        self.captures[i] = None;
                                        self.audio.detach_input(i);
                                        self.strips[i].device = d.clone();
                                        self.strips[i].meter = 0.0;
                                        self.strips[i].peak = 0.0;
                                        self.strips[i].tape_feed = i == 3 && d == virtual_tail[0];
                                        self.toast(format!("Virtual input set to {d}"));
                                    } else if let Some((stream, ip, port)) =
                                        Self::parse_vban_device(&d)
                                    {
                                        self.captures[i] = None;
                                        self.audio.detach_input(i);
                                        if i < 3
                                            && !self.strips[i].route.iter().any(|r| *r)
                                        {
                                            self.strips[i].route[0] = true;
                                        }
                                        self.set_vban_input(
                                            i,
                                            &stream,
                                            &ip,
                                            port,
                                            self.vban_quality,
                                        );
                                        self.toast(format!("VBAN input: {stream}"));
                                    } else {
                                        // Opening touches the audio service and can
                                        // block, so it happens on a worker.
                                        self.spawn_capture_open(i, d.clone());
                                        self.toast(format!("Opening {d} ..."));
                                    }
                                }
                                Picker::Bus(i) => {
                                    if d == "No device" {
                                        self.buses[i].device.clear();
                                        self.toast(format!("BUS A{}: no output device", i + 1));
                                    } else {
                                        self.buses[i].device = d.clone();
                                        self.toast(format!("Device selected: {d}"));
                                    }
                                }
                            }
                            self.picker = None;
                            close_now = true;
                        }
                    }
                    if is_strip_picker {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Rescan devices").clicked() {
                                // Enumeration blocks in the audio service when a
                                // device is wedged: ask the probe thread instead.
                                self.probe_inputs_now();
                                self.toast("Rescanning input devices ...".to_string());
                            }
                        });
                        ui.label(
                            egui::RichText::new(
                                "Missing a device? Enable it in Windows Sound settings, then rescan.",
                            )
                            .size(9.0)
                            .color(FAINT),
                        );
                    }
                        });
                });
            if !open || close_now {
                self.picker = None;
            }
        }

        if let Some(idx) = self.label_edit {
            let mut open = true;
            let mut close_now = false;
            egui::Window::new("Edit strip label")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(260.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.text_edit_singleline(&mut self.label_buf);
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            let name = self.label_buf.trim();
                            if !name.is_empty() {
                                self.strips[idx].name = name.to_string();
                            }
                            close_now = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close_now = true;
                        }
                    });
                });
            if !open || close_now {
                self.label_edit = None;
            }
        }

        if self.tape_picker {
            let mut open = self.tape_picker;
            let mut close_now = false;
            let mut browse = false;
            let mut load_path: Option<PathBuf> = None;
            let mut load_recent: Option<PathBuf> = None;
            egui::Window::new("Select Audio File")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(360.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new("Built-in demo tracks (these really play)")
                            .size(10.0)
                            .color(DIM),
                    );
                    for kind in DemoKind::ALL {
                        let label = format!("{}  -  {}", kind.file_name(), kind.description());
                        if ui.button(label).clicked() {
                            self.select_demo(kind);
                            close_now = true;
                        }
                    }
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Your own audio file (WAV, MP3, AIFF, BWF or M4A)")
                            .size(10.0)
                            .color(DIM),
                    );
                    if ui.button("Browse with Windows...").clicked() {
                        browse = true;
                    }
                    ui.label(
                        egui::RichText::new("or type/paste a full path:")
                            .size(9.0)
                            .color(DIM),
                    );
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.tape_path_input)
                                .desired_width(250.0)
                                .hint_text("C:\\Music\\track.wav"),
                        );
                        if ui.button("Load").clicked() {
                            let raw = self.tape_path_input.trim().trim_matches('"').to_string();
                            if !raw.is_empty() {
                                load_path = Some(PathBuf::from(raw));
                            }
                        }
                    });
                    if !self.tape_recent.is_empty() {
                        ui.label(egui::RichText::new("Recent").size(9.0).color(DIM));
                        for path in self.tape_recent.iter().take(4) {
                            let name = path
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.display().to_string());
                            if ui
                                .button(format!("{}  -  {}", name, path.display()))
                                .on_hover_text("Load this file again")
                                .clicked()
                            {
                                load_recent = Some(path.clone());
                            }
                        }
                    }
                    if let Some(info) = &self.tape_info {
                        ui.label(
                            egui::RichText::new(format!("Loaded: {info}"))
                                .size(9.0)
                                .color(GREEN),
                        );
                    }
                    if let Some(err) = &self.tape_error {
                        ui.separator();
                        ui.label(
                            egui::RichText::new("This file could not be loaded:")
                                .size(10.0)
                                .color(RED),
                        );
                        ui.label(egui::RichText::new(err).size(9.0).color(RED));
                        ui.label(
                            egui::RichText::new(
                                "Pick a .wav, .mp3, .aiff, .bwf or .m4a file. If it is one of those and still fails, the file may be damaged or use an unsupported codec.",
                            )
                            .size(9.0)
                            .color(DIM),
                        );
                    }
                });
            if browse {
                if let Some(path) = audio::pick_audio_file() {
                    if self.load_wav_file(path) {
                        close_now = true;
                    }
                }
            }
            if let Some(path) = load_path.or(load_recent) {
                if self.load_wav_file(path) {
                    close_now = true;
                }
            }
            self.tape_picker = open && !close_now;
        }

        if self.show_ducking {
            let mut open = self.show_ducking;
            egui::Window::new("Ducking")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(400.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.checkbox(&mut self.duck.enabled, "Enable ducking");
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.label("Source:");
                        let current = self
                            .duck
                            .source
                            .filter(|src| *src < self.active_strips)
                            .map(|src| self.strip_label(src))
                            .unwrap_or_else(|| "None".to_string());
                        egui::ComboBox::from_id_salt("duck_source")
                            .selected_text(current)
                            .width(190.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(self.duck.source.is_none(), "None")
                                    .clicked()
                                {
                                    self.duck.source = None;
                                }
                                for i in 0..self.active_strips {
                                    let label = self.strip_label(i);
                                    if ui
                                        .selectable_label(self.duck.source == Some(i), label)
                                        .clicked()
                                    {
                                        self.duck.source = Some(i);
                                        self.duck.targets &= !(1u32 << i);
                                    }
                                }
                            });
                    });
                    ui.label(
                        egui::RichText::new(
                            "The source input level keys the gain reduction (pre-fader), so its own fader does not change the sensitivity.",
                        )
                        .size(9.0)
                        .color(DIM),
                    );
                    ui.separator();
                    ui.label("Targets:");
                    egui::Grid::new("duck_targets")
                        .num_columns(3)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for i in 0..self.active_strips {
                                let mut on = (self.duck.targets & (1u32 << i)) != 0;
                                let is_source = self.duck.source == Some(i);
                                ui.add_enabled_ui(!is_source, |ui| {
                                    if ui.checkbox(&mut on, self.strip_label(i)).changed() {
                                        if on {
                                            self.duck.targets |= 1u32 << i;
                                        } else {
                                            self.duck.targets &= !(1u32 << i);
                                        }
                                    }
                                });
                                if (i + 1) % 3 == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                    ui.separator();
                    duck_param_row(
                        ui,
                        &mut self.duck.depth_db,
                        -30.0,
                        0.0,
                        0.1,
                        " dB",
                        "Depth (dB)",
                        "How far the targets drop while ducking is engaged. 0 dB = no change, -12 dB is a typical voice-over duck.",
                    );
                    duck_param_row(
                        ui,
                        &mut self.duck.threshold_db,
                        -60.0,
                        -6.0,
                        0.5,
                        " dBFS",
                        "Threshold (dBFS)",
                        "How loud the source must be before it ducks anything. Quiet sources need a lower threshold.",
                    );
                    duck_param_row(
                        ui,
                        &mut self.duck.attack_ms,
                        1.0,
                        100.0,
                        0.5,
                        " ms",
                        "Attack (ms)",
                        "How fast the targets drop. Short keeps speech intelligible, long sounds gentler.",
                    );
                    duck_param_row(
                        ui,
                        &mut self.duck.release_ms,
                        20.0,
                        1000.0,
                        2.0,
                        " ms",
                        "Release (ms)",
                        "How long the targets stay down after the source goes quiet. Short recovers fast, long holds through pauses.",
                    );

                    ui.separator();
                    // Live levels. Dragging anywhere on either bar sets the
                    // threshold, so the marker and the slider stay in sync.
                    let source_level = self.audio.duck_source_level();
                    let target_level = (0..self.active_strips)
                        .filter(|i| (self.duck.targets & (1u32 << i)) != 0)
                        .map(|i| self.audio.strip_peak(i))
                        .fold(0.0f32, f32::max);
                    let mut threshold = self.duck.threshold_db;
                    let mut depth = self.duck.depth_db;
                    let threshold_changed = duck_meter(
                        ui,
                        "Source Volume",
                        source_level,
                        Some(DuckMarker {
                            value: &mut threshold,
                            bar_lo: -60.0,
                            bar_hi: 0.0,
                            clamp_lo: -60.0,
                            clamp_hi: -6.0,
                            hint: "Drag to set the threshold",
                        }),
                    );
                    let depth_changed = duck_meter(
                        ui,
                        "Target Volume",
                        target_level,
                        Some(DuckMarker {
                            value: &mut depth,
                            bar_lo: -30.0,
                            bar_hi: 0.0,
                            clamp_lo: -30.0,
                            clamp_hi: 0.0,
                            hint: "Drag to set the depth",
                        }),
                    );
                    if threshold_changed {
                        self.duck.threshold_db = threshold;
                    }
                    if depth_changed {
                        self.duck.depth_db = depth;
                    }

                    ui.separator();
                    let reduction = self.audio.duck_gain_db();
                    ui.label(format!("Gain reduction: {reduction:.1} dB"));
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width().min(320.0), 8.0),
                        Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 2.0, TRACK);
                    let fraction = (-reduction / 30.0).clamp(0.0, 1.0);
                    ui.painter().rect_filled(
                        Rect::from_min_size(
                            rect.left_top(),
                            Vec2::new(rect.width() * fraction, rect.height()),
                        ),
                        2.0,
                        AMBER,
                    );
                    if !self.duck.enabled {
                        ui.label(
                            egui::RichText::new("Ducking is off")
                                .size(9.0)
                                .color(DIM),
                        );
                    } else if self.duck.source.is_none() {
                        ui.label(
                            egui::RichText::new("Choose a source strip to key the ducking")
                                .size(9.0)
                                .color(DIM),
                        );
                    }
                });
            self.show_ducking = open;
        }

        if let Some(index) = self.bus_rename {
            let mut open = true;
            let mut close = false;
            let current = self.buses[index].name.clone();
            egui::Window::new("Rename bus")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(240.0)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!("BUS {current}"))
                            .size(10.0)
                            .color(DIM),
                    );
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.bus_rename_text)
                            .desired_width(220.0)
                            .char_limit(16),
                    );
                    if self.bus_rename_focus {
                        edit.request_focus();
                        self.bus_rename_focus = false;
                    }
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() || enter {
                            let name: String = self.bus_rename_text.trim().chars().take(16).collect();
                            if !name.is_empty() {
                                self.buses[index].name = name;
                            }
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if close || !open {
                self.bus_rename = None;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Screenshot
    // -----------------------------------------------------------------------

    fn handle_shot(&mut self, ctx: &egui::Context) {
        if self.shot_path.is_none() {
            return;
        }
        self.frame_no += 1;
        ctx.request_repaint();
        if self.frame_no == 120 && !self.shot_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            self.shot_requested = true;
        }
        if self.shot_requested {
            let events = ctx.input(|i| i.events.clone());
            for ev in events {
                if let egui::Event::Screenshot { image, .. } = ev {
                    if let Some(path) = &self.shot_path {
                        match save_png(path, &image) {
                            Ok(()) => eprintln!("screenshot saved to {}", path.display()),
                            Err(e) => eprintln!("screenshot failed: {e}"),
                        }
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }
}

impl eframe::App for MixerApp {
    /// WM_DESTROY in the original writes the scene twice (FUN_00529c80 at
    /// 0x54beb1 and 0x54bec2). Do the same on a normal close so every strip,
    /// including HW INPUT 1-3 and the virtual inputs, comes back next time.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for slot in &mut self.vsts {
            slot.chain.close_editor();
        }
        self.vc.stop();
        if self.save_on_exit && self.shot_path.is_none() {
            self.save_settings();
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.first_frame {
            self.first_frame = false;
            if self.shot_path.is_some() {
                ctx.request_repaint();
            }
        }
        if self.mixer_hwnd == 0 {
            if let Ok(handle) = frame.window_handle() {
                if let raw_window_handle::RawWindowHandle::Win32(window) = handle.as_raw() {
                    self.mixer_hwnd = window.hwnd.get() as isize;
                    diag_log(format!("main: mixer hwnd {:#x}", self.mixer_hwnd));
                }
            }
        }
        if self.pending_maximize {
            self.pending_maximize = false;
            if self.shot_path.is_none() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            }
        }
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        self.time += dt as f64;

        // Remember the window size and position for the next run. Maximized
        // sizes are ignored so Restore returns to the last manual size, but
        // the maximized state itself is saved.
        let viewport = ctx.input(|i| i.viewport().clone());
        self.window_maximized = viewport.maximized.unwrap_or(false);
        self.ui_zoom = ctx.zoom_factor();
        if viewport.maximized != Some(true) {
            if let Some(inner) = viewport.inner_rect {
                let (w, h) = (inner.width(), inner.height());
                if w >= 400.0 && h >= 300.0 {
                    self.window_size = Some([w, h]);
                }
            }
            if let Some(outer) = viewport.outer_rect {
                self.window_pos = Some([outer.min.x, outer.min.y]);
            }
        }
        self.advance_startup(dt as f64);
        self.poll_vst_scan();
        if let Some(slot) = self.open_editor_once {
            if self.mixer_hwnd != 0 && self.time > 1.0 {
                self.open_editor_once = None;
                eprintln!("[repro] opening editor for slot {slot}");
                self.open_vst_editor(slot);
                eprintln!("[repro] open_vst_editor returned");
            }
        }
        // Keep every open plug-in window alive (resize requests and its close
        // button), without ever drawing its settings inside the mixer.
        for slot in &mut self.vsts {
            slot.chain.service_editor();
        }
        // The wrapper's chain is read twice a second: cheap (one parameter) and
        // fast enough that adding a plug-in inside it shows up almost at once.
        for slot in &mut self.vsts {
            if !slot.chain.has_plugin() {
                if !slot.sub_plugins.is_empty() {
                    slot.sub_plugins.clear();
                }
                continue;
            }
            if self.time - slot.sub_plugins_at >= 0.5 {
                slot.sub_plugins_at = self.time;
                if let Some(list) = slot.chain.chain_summary() {
                    if slot.sub_plugins != list {
                        slot.sub_plugins = list;
                    }
                }
            }
        }
        if let Some(path) = self.vst_shot.clone() {
            // Wait for a loud moment: LoudMax's meters span -30..0 dBFS, so a
            // quiet passage draws nothing even when the plug-in is fine.
            let loud = self
                .vsts
                .iter()
                .map(|slot| slot.chain.input_peak())
                .fold(0.0f32, f32::max);
            if self.time > 4.0 && loud > 0.12 {
                self.vst_shot = None;
                for slot in &self.vsts {
                    if slot.chain.editor_is_open() {
                        let outcome = slot.chain.snapshot_editor(&path);
                        diag_log(format!("vst shot {path} at peak {loud:.3}: {outcome:?}"));
                    }
                }
            }
        }
        if self.vst_log {
            self.vst_log_clock += dt as f64;
            if self.vst_log_clock >= 1.0 {
                self.vst_log_clock = 0.0;
                for (index, slot) in self.vsts.iter().enumerate() {
                    for i in 0..self.active_strips {
                        diag_log(format!(
                            "strip[{i}] capture={} device='{}' route={:?} mute={} solo={} meter={:.3}",
                            self.captures[i].is_some(),
                            self.strips[i].device,
                            self.strips[i].route,
                            self.strips[i].mute,
                            self.strips[i].solo,
                            self.strips[i].meter
                        ));
                    }
                    if slot.chain.has_plugin() {
                        let label = self.strip_label(index);
                        let name = slot
                            .info
                            .as_ref()
                            .map(|i| i.name.clone())
                            .unwrap_or_default();
                        diag_log(format!(
                            "vst[{index}] '{label}' '{name}' {}",
                            slot.chain.diag()
                        ));
                    }
                }
            }
        }
        if let Some(limit) = self.auto_exit {
            if self.time >= limit {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // A screenshot run should capture the clean console, not a tooltip
        // left under wherever the desktop cursor happens to rest.
        if self.shot_path.is_some() {
            ctx.input_mut(|i| i.pointer = egui::PointerState::default());
        }

        if !self.lock_gui && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::R)) {
            self.restart_engine();
        }
        if !self.lock_gui && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::S)) {
            self.save_settings();
        }
        if !self.lock_gui && ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::O)) {
            self.load_settings(true);
        }
        if !self.lock_gui
            && ctx.input(|i| i.modifiers.ctrl && i.modifiers.shift && i.key_pressed(egui::Key::V))
        {
            self.show_vc = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.menu_open = false;
            self.show_system = false;
            self.show_vc = false;
            self.show_about = false;
            self.show_tape_opts = false;
            self.show_presets = false;
            self.bus_eq_open = None;
            self.picker = None;
            self.label_edit = None;
            self.tape_picker = false;
            self.show_info = None;
        }

        // Recorder progress: the audio callback owns the play position, so the
        // counter follows what is actually being heard. If no device exists the
        // old simulated counter still moves for the picture check.
        if self.tape.playing {
            if self.audio.available && self.audio.has_source() {
                self.tape.pos = self.audio.position();
                if self.audio.take_finished() {
                    self.tape.playing = false;
                    self.tape.pos = self.tape.len;
                } else if !self.audio.is_playing() {
                    self.tape.playing = false;
                }
            } else {
                self.tape.pos += dt;
                if self.tape.pos >= self.tape.len {
                    self.tape.pos = 0.0;
                    self.tape.playing = false;
                }
            }
        }
        if self.tape.recording {
            self.tape.pos = (self.tape.pos + dt).min(600.0);
        }

        // An audio file dropped on the console is loaded into the tape deck.
        if !self.lock_gui {
            let dropped: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            if let Some(path) = dropped.into_iter().next() {
                self.load_wav_file(path);
            }
        }

        self.update_meters(dt);
        if self.meter_log {
            self.meter_clock += dt as f64;
            if self.meter_clock >= 1.0 {
                self.meter_clock = 0.0;
                let mut row = String::new();
                for i in 0..self.active_strips {
                    let (name, raw) = match self.captures.get(i).and_then(|c| c.as_ref()) {
                        Some(capture) => (capture.device().to_string(), capture.peak()),
                        None => ("-".to_string(), 0.0),
                    };
                    row.push_str(&format!(
                        "{}='{}' raw={:.3} meter={:.3}  ",
                        i + 1,
                        name,
                        raw,
                        self.strips[i].meter
                    ));
                }
                diag_log(format!("meter: {row}"));
            }
        }
        self.poll_audio_devices(dt as f64);
        self.poll_input_devices();
        self.poll_capture_jobs();
        self.sync_app_targets();

        egui::TopBottomPanel::top("topbar")
            .exact_height(38.0)
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                self.draw_top_bar(ui);
            });
        egui::TopBottomPanel::bottom("bottombar")
            .exact_height(72.0)
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                self.draw_bottom(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let pad = 8.0;
                let gap = 4.0;
                let gap_mid = 8.0;
                let bank_h = 18.0;
                let bank_top = rect.top() + 4.0;

                let mut last_bank = if self.active_strips < MAX_STRIPS {
                    Self::bank_for_strip(self.active_strips)
                } else {
                    Self::bank_for_strip(self.active_strips - 1)
                };
                let mut bx = rect.left() + pad;
                for bank in 0..=last_bank {
                    let chip = Rect::from_min_size(
                        Pos2::new(bx, bank_top),
                        Vec2::new(56.0, bank_h),
                    );
                    if top_button(
                        ui,
                        chip,
                        &format!("BANK {}", bank + 1),
                        self.strip_bank == bank,
                        AMBER,
                    ) {
                        self.strip_bank = bank;
                    }
                    bx += 56.0 + 4.0;
                }

                // STREAMER MODE: a running light across every fader. It sits
                // left of the ADD button; right click sets the chase speed.
                let streamer_rect = Rect::from_min_size(
                    Pos2::new(rect.right() - pad - 132.0 - 8.0 - 112.0, bank_top),
                    Vec2::new(112.0, bank_h),
                );
                let streamer_sense = if self.lock_gui {
                    Sense::hover()
                } else {
                    Sense::click()
                };
                let streamer_resp = ui.interact(
                    streamer_rect,
                    Id::new("streamer_mode"),
                    streamer_sense,
                );
                let streamer_on = self.streamer_mode;
                ui.painter().rect_filled(
                    streamer_rect,
                    3.0,
                    if streamer_on {
                        GREEN.gamma_multiply(0.22)
                    } else if streamer_resp.hovered() {
                        PANEL_HI
                    } else {
                        Color32::from_rgb(27, 31, 38)
                    },
                );
                ui.painter().rect_stroke(
                    streamer_rect,
                    3.0,
                    Stroke::new(1.0, if streamer_on { GREEN } else { LINE }),
                );
                ui.painter().text(
                    streamer_rect.center(),
                    Align2::CENTER_CENTER,
                    "STREAMER MODE",
                    FontId::proportional(8.0),
                    if streamer_on { GREEN } else { TEXT },
                );
                let mut streamer_toggled = false;
                if streamer_resp.clicked() {
                    self.streamer_mode = !self.streamer_mode;
                    streamer_toggled = true;
                }
                let streamer_resp = streamer_resp.on_hover_text(format!(
                    "Running light across every fader ({:.0}/s). Right click for speed.",
                    self.streamer_speed
                ));
                streamer_resp.context_menu(|ui| {
                    ui.label("Chase speed");
                    for (label, speed) in [
                        ("Slow (4/s)", 4.0f32),
                        ("Normal (7/s)", 7.0),
                        ("Fast (11/s)", 11.0),
                        ("Very fast (16/s)", 16.0),
                    ] {
                        if ui
                            .selectable_label((self.streamer_speed - speed).abs() < 0.1, label)
                            .clicked()
                        {
                            self.streamer_speed = speed;
                            ui.close_menu();
                        }
                    }
                });
                if streamer_toggled {
                    self.toast(if self.streamer_mode {
                        format!("Streamer mode on ({:.0}/s)", self.streamer_speed)
                    } else {
                        "Streamer mode off".to_string()
                    });
                }

                let can_add = self.active_strips < MAX_STRIPS;
                let add_rect = Rect::from_min_size(
                    Pos2::new(rect.right() - pad - 132.0, bank_top),
                    Vec2::new(132.0, bank_h),
                );
                let add_label = if can_add {
                    "+ ADD VIRTUAL INPUT"
                } else {
                    "INPUT LIMIT"
                };
                if top_button(ui, add_rect, add_label, false, AMBER) && can_add {
                    self.add_virtual_input();
                    last_bank = if self.active_strips < MAX_STRIPS {
                        Self::bank_for_strip(self.active_strips)
                    } else {
                        Self::bank_for_strip(self.active_strips - 1)
                    };
                }
                self.strip_bank = self.strip_bank.min(last_bank);

                let total_w = rect.width() - pad * 2.0 - gap * 9.0 - gap_mid;
                let card_w =
                    (total_w / (10.0 + (audio::MAX_BUSES - 5) as f32)).floor().max(60.0);
                let top = bank_top + bank_h + 4.0;
                let h = rect.bottom() - top - 5.0;
                let start = Self::bank_start(self.strip_bank);
                let mut x = rect.left() + pad;
                for slot in 0..5 {
                    let idx = start + slot;
                    let card = Rect::from_min_size(Pos2::new(x, top), Vec2::new(card_w, h));
                    if idx < self.active_strips {
                        self.draw_strip_card(ui, card, idx);
                    } else {
                        ui.painter().rect_filled(card, 5.0, Color32::from_rgb(18, 21, 26));
                        ui.painter()
                            .rect_stroke(card, 5.0, Stroke::new(1.0, LINE));
                        if idx == self.active_strips && self.active_strips < MAX_STRIPS {
                            let r = ui.interact(
                                card,
                                Id::new(("add_strip_card", idx)),
                                Sense::click(),
                            );
                            ui.painter().text(
                                card.center(),
                                Align2::CENTER_CENTER,
                                "+",
                                FontId::proportional(24.0),
                                FAINT,
                            );
                            ui.painter().text(
                                Pos2::new(card.center().x, card.center().y + 18.0),
                                Align2::CENTER_CENTER,
                                "ADD VIRTUAL INPUT",
                                FontId::proportional(8.0),
                                FAINT,
                            );
                            if r.clicked() {
                                self.add_virtual_input();
                            }
                        }
                    }
                    x += card_w + gap;
                }
                x += gap_mid - gap;
                let loops = self.feedback_loops();
                for i in 0..audio::MAX_BUSES {
                    let card = Rect::from_min_size(Pos2::new(x, top), Vec2::new(card_w, h));
                    self.draw_bus_card(ui, card, i, &loops[i]);
                    x += card_w + gap;
                }
            });

        self.draw_menu(ctx);
        self.draw_dialogs(ctx);
        self.sync_audio_controls();
        self.draw_resize_grip(ctx);
        self.handle_shot(ctx);

        // 60 fps so the smoothed meters move continuously; the original's
        // timer also ran every 15 ms.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// Square-outline icon button used for maximize/restore in the title bar.
fn top_icon_button(ui: &egui::Ui, rect: Rect, maximized: bool) -> bool {
    let resp = ui.interact(rect, Id::new("topbtn_max"), Sense::click());
    let fill = if resp.hovered() {
        PANEL_HI
    } else {
        Color32::from_rgb(27, 31, 38)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, fill);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.0, LINE));
    let center = rect.center();
    if maximized {
        // Restore: two overlapping squares.
        let back = Rect::from_center_size(center + Vec2::new(1.5, -1.5), Vec2::splat(8.0));
        painter.rect_stroke(back, 1.0, Stroke::new(1.0, TEXT));
        let front = Rect::from_center_size(center + Vec2::new(-1.5, 1.5), Vec2::splat(8.0));
        painter.rect_filled(front, 1.0, fill);
        painter.rect_stroke(front, 1.0, Stroke::new(1.0, TEXT));
    } else {
        painter.rect_stroke(
            Rect::from_center_size(center, Vec2::splat(9.0)),
            1.0,
            Stroke::new(1.0, TEXT),
        );
    }
    resp.on_hover_text(if maximized { "Restore" } else { "Maximize" })
        .clicked()
}

fn top_button(ui: &egui::Ui, rect: Rect, label: &str, active: bool, accent: Color32) -> bool {
    let resp = ui.interact(rect, Id::new(("topbtn", label)), Sense::click());
    let hovered = resp.hovered();
    let fill = if active {
        accent.gamma_multiply(0.85)
    } else if hovered {
        PANEL_HI
    } else {
        Color32::from_rgb(27, 31, 38)
    };
    ui.painter().rect_filled(rect, 3.0, fill);
    ui.painter().rect_stroke(
        rect,
        3.0,
        Stroke::new(1.0, if active { accent } else { LINE }),
    );
    let color = if active {
        Color32::from_rgb(20, 20, 24)
    } else {
        TEXT
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(9.0),
        color,
    );
    resp.clicked()
}

fn mode_chip(
    ui: &egui::Ui,
    rect: Rect,
    label: &str,
    accent: Color32,
    enabled: bool,
    id: Id,
    tip: &str,
) -> (bool, Response) {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let hovered = resp.hovered() && enabled;
    ui.painter().rect_filled(
        rect,
        2.5,
        if hovered {
            Color32::from_rgb(42, 48, 58)
        } else {
            Color32::from_rgb(27, 31, 38)
        },
    );
    ui.painter()
        .rect_stroke(rect, 2.5, Stroke::new(1.0, accent.gamma_multiply(0.55)));
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(7.6),
        if enabled { accent } else { FAINT },
    );
    let clicked = resp.clicked() && enabled;
    let resp = resp.on_hover_text(tip);
    (clicked, resp)
}

fn draw_bus_eq_graph(
    ui: &egui::Ui,
    rect: Rect,
    bands: &mut [EqBand; 6],
    accent: Color32,
    id: Id,
    enabled: bool,
) {
    let p = ui.painter();
    p.rect_filled(rect, 3.0, TRACK);
    p.rect_stroke(rect, 3.0, Stroke::new(1.0, LINE));
    p.text(
        rect.left_top() + Vec2::new(3.0, 2.0),
        Align2::LEFT_TOP,
        "6-BAND EQ",
        FontId::proportional(5.8),
        FAINT,
    );
    let graph = Rect::from_min_max(
        Pos2::new(rect.left() + 5.0, rect.top() + 13.0),
        Pos2::new(rect.right() - 5.0, rect.bottom() - 5.0),
    );
    // zero line and +/- grid
    for (v, strong) in [(0.0f32, true), (9.0, false), (-9.0, false)] {
        let y = graph.center().y - v / 18.0 * graph.height() * 0.5;
        p.line_segment(
            [Pos2::new(graph.left(), y), Pos2::new(graph.right(), y)],
            Stroke::new(
                1.0,
                if strong { LINE } else { LINE.gamma_multiply(0.45) },
            ),
        );
    }
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let resp = ui.interact(graph, id, sense);
    if enabled && resp.dragged() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let t = ((pos.x - graph.left()) / graph.width()).clamp(0.0, 1.0);
            let idx = ((t * 5.0).round() as usize).min(5);
            let g = ((graph.center().y - pos.y) / (graph.height() * 0.5) * 18.0)
                .clamp(-18.0, 18.0);
            bands[idx].gain = g;
            bands[idx].on = true;
        }
    }
    let points: Vec<Pos2> = bands
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let x = graph.left() + graph.width() * (i as f32 / 5.0);
            let y = graph.center().y - b.gain / 18.0 * graph.height() * 0.5;
            Pos2::new(x, y)
        })
        .collect();
    let any_on = bands.iter().any(|b| b.on);
    for w in points.windows(2) {
        p.line_segment(
            [w[0], w[1]],
            Stroke::new(
                1.2,
                if any_on {
                    accent.gamma_multiply(0.55)
                } else {
                    FAINT
                },
            ),
        );
    }
    for (i, pt) in points.iter().enumerate() {
        let on = bands[i].on;
        p.circle_filled(
            *pt,
            2.6,
            if on {
                accent
            } else {
                FAINT.gamma_multiply(0.7)
            },
        );
    }
    if resp.hovered() {
        p.rect_stroke(graph, 0.0, Stroke::new(1.0, accent.gamma_multiply(0.4)));
    }
    let _ = resp.on_hover_text("6-band EQ curve: drag a band up or down, right click the EQ button for the full editor");
}

fn chip(
    ui: &egui::Ui,
    rect: Rect,
    label: &str,
    active: bool,
    accent: Color32,
    enabled: bool,
    id: Id,
    tip: &str,
) -> (bool, Response) {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let hovered = resp.hovered() && enabled;
    let fill = if active {
        accent.gamma_multiply(if hovered { 1.0 } else { 0.88 })
    } else if hovered {
        Color32::from_rgb(46, 53, 65)
    } else {
        Color32::from_rgb(34, 39, 48)
    };
    ui.painter().rect_filled(rect, 2.5, fill);
    if active || hovered {
        ui.painter()
            .rect_stroke(rect, 2.5, Stroke::new(1.0, accent.gamma_multiply(0.9)));
    }
    let color = if active {
        Color32::from_rgb(16, 17, 20)
    } else if enabled {
        TEXT
    } else {
        FAINT
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(7.4),
        color,
    );
    let clicked = resp.clicked() && enabled;
    let resp = resp.on_hover_text(tip);
    (clicked, resp)
}

fn menu_item(ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool) -> bool {
    let h = 24.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let hovered = resp.hovered() && enabled;
    if hovered {
        ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(52, 60, 74));
    }
    ui.painter().text(
        rect.left_center() + Vec2::new(9.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        if enabled { TEXT } else { FAINT },
    );
    if !shortcut.is_empty() {
        ui.painter().text(
            rect.right_center() - Vec2::new(9.0, 0.0),
            Align2::RIGHT_CENTER,
            shortcut,
            FontId::proportional(9.5),
            FAINT,
        );
    }
    resp.clicked() && enabled
}

fn check_item(ui: &mut egui::Ui, label: &str, checked: bool) -> bool {
    let h = 24.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(52, 60, 74));
    }
    ui.painter().text(
        rect.left_center() + Vec2::new(9.0, 0.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        TEXT,
    );
    let box_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 16.0, rect.center().y),
        Vec2::splat(12.0),
    );
    ui.painter().rect_filled(box_rect, 2.0, Color32::from_rgb(22, 25, 31));
    ui.painter()
        .rect_stroke(box_rect, 2.0, Stroke::new(1.0, FAINT));
    if checked {
        ui.painter().circle_filled(box_rect.center(), 3.5, AMBER);
    }
    resp.clicked()
}

fn draw_knob(
    ui: &egui::Ui,
    center: Pos2,
    radius: f32,
    value: &mut f32,
    min: f32,
    max: f32,
    default: f32,
    accent: Color32,
    id: Id,
    enabled: bool,
    tip: &str,
) -> bool {
    let rect = Rect::from_center_size(center, Vec2::splat(radius * 2.4));
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let mut changed = false;
    // FUN_00541ce0 knobs (0x10b-0x136) use
    //   new = value_at_grab + (grab_y - y) * 0.1
    // i.e. upward movement increases the value by 0.1 per pixel.
    let grab_id = id.with("knob-grab");
    if enabled && resp.drag_started() {
        let y = ui.input(|i| {
            i.pointer
                .press_origin()
                .or_else(|| i.pointer.interact_pos())
                .map(|p| p.y)
                .unwrap_or(0.0)
        });
        ui.ctx().data_mut(|d| d.insert_temp(grab_id, (*value, y)));
    }
    if enabled && resp.dragged() {
        if let Some((value_at_grab, grab_y)) =
            ui.ctx().data(|d| d.get_temp::<(f32, f32)>(grab_id))
        {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                *value = (value_at_grab + (grab_y - pos.y) * KNOB_STEP_PER_PX)
                    .clamp(min, max);
            }
        }
        changed = true;
    }
    if enabled && resp.double_clicked() {
        *value = default;
        changed = true;
    }
    if enabled && resp.hovered() {
        // FUN_00542a60 only handles the ten faders (0xd4-0xdd); the original
        // has no wheel case for knobs, EQ bands, compressors or gates.
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    let p = ui.painter();
    p.circle_filled(center, radius + 2.0, Color32::from_rgb(20, 23, 29));
    p.circle_stroke(center, radius + 2.0, Stroke::new(1.0, LINE));
    p.circle_filled(center, radius, TRACK);
    let t = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let ang = (135.0 + t * 270.0).to_radians();
    let dir = Vec2::new(ang.cos(), ang.sin());
    let col = if enabled { accent } else { FAINT };
    p.line_segment(
        [center + dir * radius * 0.25, center + dir * radius * 0.85],
        Stroke::new(2.0, col),
    );
    p.circle_filled(center + dir * radius * 0.25, 1.3, col);
    // arc ticks at the ends
    for a in [135.0f32, 405.0] {
        let d = Vec2::new(a.to_radians().cos(), a.to_radians().sin());
        p.line_segment(
            [center + d * (radius + 1.0), center + d * (radius + 3.0)],
            Stroke::new(1.0, FAINT),
        );
    }
    if resp.hovered() {
        p.circle_stroke(center, radius + 2.0, Stroke::new(1.0, accent));
    }
    let _ = resp.on_hover_text(tip);
    changed
}


fn fader(
    ui: &egui::Ui,
    rect: Rect,
    value: &mut f32,
    _meter: f32,
    _peak: f32,
    _limit: &mut f32,
    accent: Color32,
    id: Id,
    enabled: bool,
    is_strip: bool,
    max_db: f32,
    streamer: f32,
    tip: &str,
) {
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let pad = if is_strip { STRIP_FADER_PAD } else { BUS_FADER_PAD };
    let travel = fader_travel(rect, pad);
    // FUN_00541ce0 remembers the gain and pointer y at mouse-down and then uses
    //   new = gain_at_grab - (y - grab_y) * 72.0 / (control_height - pad)
    // so the value never accumulates error against a clamp.
    let grab_id = id.with("fader-grab");
    if enabled && resp.drag_started() {
        let y = ui.input(|i| {
            i.pointer
                .press_origin()
                .or_else(|| i.pointer.interact_pos())
                .map(|p| p.y)
                .unwrap_or(0.0)
        });
        ui.ctx().data_mut(|d| d.insert_temp(grab_id, (*value, y)));
    }
    if enabled && resp.dragged() {
        if let Some((gain_at_grab, grab_y)) =
            ui.ctx().data(|d| d.get_temp::<(f32, f32)>(grab_id))
        {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let span = travel.height().max(1.0);
                *value = (gain_at_grab - (pos.y - grab_y) * FADER_TRAVEL_DB / span)
                    .clamp(GAIN_MIN, max_db);
            }
        }
    }
    if enabled && resp.double_clicked() {
        *value = 0.0;
    }
    if enabled && resp.hovered() {
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if scroll != 0.0 {
            *value = (*value + scroll.signum() * GAIN_STEP).clamp(GAIN_MIN, max_db);
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    let p = ui.painter();
    let label_w = 13.0;
    let fader_area = Rect::from_min_max(
        Pos2::new(rect.left() + label_w, rect.top()),
        rect.max,
    );
    let track = Rect::from_center_size(
        Pos2::new(fader_area.center().x, fader_area.center().y),
        Vec2::new(7.0, rect.height() - 8.0),
    );
    p.rect_filled(track, 2.0, TRACK);
    p.rect_stroke(track, 2.0, Stroke::new(1.0, LINE));
    // ticks every 6 dB
    let mut db = GAIN_MIN;
    while db <= GAIN_MAX + 0.01 {
        let y = gain_y(travel, db);
        let len = if db == 0.0 { 5.0 } else { 3.0 };
        p.line_segment(
            [
                Pos2::new(track.right() + 1.0, y),
                Pos2::new(track.right() + 1.0 + len, y),
            ],
            Stroke::new(1.0, if db == 0.0 { DIM } else { FAINT.gamma_multiply(0.55) }),
        );
        db += 6.0;
    }
    // dB scale labels
    for (db, label) in [
        (12.0f32, "+12"),
        (0.0, "0"),
        (-12.0, "-12"),
        (-24.0, "-24"),
        (-36.0, "-36"),
        (-48.0, "-48"),
        (-60.0, "-60"),
    ] {
        let y = gain_y(travel, db);
        p.text(
            Pos2::new(rect.left() + label_w - 3.0, y),
            Align2::RIGHT_CENTER,
            label,
            FontId::proportional(6.0),
            if db == 0.0 { DIM } else { FAINT },
        );
    }
    let y = gain_y(travel, *value);
    let handle = Rect::from_center_size(
        Pos2::new(fader_area.center().x, y),
        Vec2::new(fader_area.width() - 2.0, 8.0),
    );
    p.rect_filled(handle, 2.0, Color32::from_rgb(40, 45, 55));
    p.rect_stroke(handle, 2.0, Stroke::new(1.0, accent));
    p.line_segment(
        [
            Pos2::new(handle.left() + 3.0, handle.center().y),
            Pos2::new(handle.right() - 3.0, handle.center().y),
        ],
        Stroke::new(2.0, accent),
    );
    if resp.hovered() {
        p.rect_stroke(track, 2.0, Stroke::new(1.0, accent.gamma_multiply(0.6)));
    }
    if streamer > 0.0 {
        // Streamer mode: a halo plus a stronger outline, and a brighter knob
        // line, so the chase reads like a running light.
        p.rect_filled(
            track.expand(2.0),
            3.0,
            accent.gamma_multiply(0.18 * streamer),
        );
        p.rect_stroke(
            track,
            2.0,
            Stroke::new(1.5, accent.gamma_multiply(0.9 * streamer)),
        );
        p.line_segment(
            [
                Pos2::new(handle.left() + 3.0, handle.center().y),
                Pos2::new(handle.right() - 3.0, handle.center().y),
            ],
            Stroke::new(2.0, accent.gamma_multiply(0.5 + 0.5 * streamer)),
        );
    }
    if !enabled {
        p.rect_filled(track, 2.0, TRACK.gamma_multiply(0.7));
    }
    let resp = resp.on_hover_text(tip);
    if enabled {
        resp.context_menu(|ui| {
            if ui.button("Reset to 0 dB").clicked() {
                *value = 0.0;
                ui.close_menu();
            }
            if ui.button("Reset to -60 dB").clicked() {
                *value = GAIN_MIN;
                ui.close_menu();
            }
            ui.separator();
            ui.label(format!("{}", fmt_db(*value)));
        });
    }
}

/// One ducking parameter: compact bar slider, editable value, name, and a
/// one-line explanation of what it does.
fn duck_param_row(
    ui: &mut egui::Ui,
    value: &mut f32,
    lo: f32,
    hi: f32,
    speed: f32,
    suffix: &str,
    name: &str,
    hint: &str,
) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [150.0, 18.0],
            egui::Slider::new(value, lo..=hi).show_value(false),
        );
        ui.add(
            egui::DragValue::new(value)
                .range(lo..=hi)
                .speed(speed)
                .max_decimals(1)
                .suffix(suffix),
        );
        ui.label(egui::RichText::new(name).color(TEXT));
    });
    ui.label(egui::RichText::new(hint).size(9.0).color(DIM));
}

/// A draggable marker drawn on a level meter. `bar` is the value span the bar
/// covers, `clamp` the range the marker may actually take.
struct DuckMarker<'a> {
    value: &'a mut f32,
    bar_lo: f32,
    bar_hi: f32,
    clamp_lo: f32,
    clamp_hi: f32,
    hint: &'static str,
}

/// Horizontal level bar on a -60..0 dBFS scale with an optional draggable
/// marker. Returns true when the marker moved its value.
fn duck_meter(
    ui: &mut egui::Ui,
    name: &str,
    level: f32,
    marker: Option<DuckMarker<'_>>,
) -> bool {
    let level_db = lin_to_db(level).clamp(-60.0, 0.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(name).size(10.0).color(TEXT));
        ui.label(
            egui::RichText::new(format!("{level_db:.1} dB"))
                .size(9.0)
                .color(DIM),
        );
    });

    let sense = if marker.is_some() {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 12.0), sense);
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, TRACK);
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, LINE));

    let fill = ((level_db + 60.0) / 60.0).clamp(0.0, 1.0);
    if fill > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(
                rect.left_top(),
                Vec2::new(rect.width() * fill, rect.height()),
            ),
            2.0,
            GREEN,
        );
    }

    let Some(marker) = marker else {
        return false;
    };

    let span = (marker.bar_hi - marker.bar_lo).abs().max(1.0);
    let tick = ((*marker.value - marker.bar_lo) / span).clamp(0.0, 1.0);
    let x = rect.left() + rect.width() * tick;
    painter.line_segment(
        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
        Stroke::new(2.0, AMBER),
    );

    let mut changed = false;
    if response.dragged() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            let value = marker.bar_lo + t * span;
            *marker.value = value.clamp(marker.clamp_lo, marker.clamp_hi);
            changed = true;
        }
    }
    response.on_hover_text(marker.hint);
    changed
}

fn meter_widget(
    ui: &egui::Ui,
    rect: Rect,
    meter: f32,
    peak: f32,
    limit: &mut f32,
    id: Id,
    enabled: bool,
    min_db: f32,
    span_db: f32,
    pad: f32,
) {
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let p = ui.painter();
    p.rect_filled(rect, 1.5, TRACK);
    p.rect_stroke(rect, 1.5, Stroke::new(1.0, LINE));

    // The original meter bitmap is a separate control from the fader, but the
    // rebuild draws both in one column, so the bar uses the fader's travel
    // rectangle. That puts +12 dB at the same pixel as the fader's +12 tick
    // and 0 dB within a few pixels of the fader's 0, instead of the bar
    // floating several dB above its own printed scale.
    let bar = fader_travel(rect, pad);
    let level_db = lin_to_db(meter).max(METER_FLOOR_DB).clamp(min_db, min_db + span_db);
    let seg_h = 4.0f32;
    let gap = 1.0f32;
    let usable = bar.height() - 2.0;
    let count = (usable / (seg_h + gap)).floor().max(1.0) as usize;
    // Continuous level position: the top segment is partly filled instead of
    // being switched on one whole segment at a time. That keeps the meter's
    // segmented look but lets the bar move smoothly, as a VU needle does.
    let level_t = ((level_db - min_db) / span_db).clamp(0.0, 1.0);
    let fill_y = bar.bottom() - 1.0 - level_t * usable;
    for i in 0..count {
        let y_bottom = bar.bottom() - 1.0 - i as f32 * (seg_h + gap);
        let y_top = y_bottom - seg_h;
        let seg_center_t = (i as f32 + 0.5) / count as f32;
        let db = min_db + seg_center_t * span_db;
        let col = if db > -3.0 {
            RED
        } else if db > -12.0 {
            YELLOW
        } else {
            GREEN
        };
        let seg = Rect::from_min_max(
            Pos2::new(rect.left() + 1.0, y_top),
            Pos2::new(rect.right() - 1.0, y_bottom),
        );
        p.rect_filled(seg, 0.5, METER_UNLIT);
        if y_bottom > fill_y {
            let lit_top = y_top.max(fill_y);
            p.rect_filled(
                Rect::from_min_max(
                    Pos2::new(rect.left() + 1.0, lit_top),
                    Pos2::new(rect.right() - 1.0, y_bottom),
                ),
                0.5,
                col.gamma_multiply(0.9),
            );
        }
    }
    // peak hold
    if peak > 0.001 {
        let pdb = lin_to_db(peak).clamp(min_db, min_db + span_db);
        let pt = ((pdb - min_db) / span_db).clamp(0.0, 1.0);
        let py = bar.bottom() - 1.0 - pt * usable;
        p.line_segment(
            [Pos2::new(rect.left() + 1.0, py), Pos2::new(rect.right() - 1.0, py)],
            Stroke::new(1.0, Color32::from_rgb(240, 240, 210)),
        );
    }
    // brickwall limiter threshold
    let ly = gain_y(bar, *limit);
    p.line_segment(
        [Pos2::new(rect.left(), ly), Pos2::new(rect.left() + 4.0, ly)],
        Stroke::new(1.5, RED),
    );
    // FUN_00541ce0 meter cells 200-205 (0xc8-0xcd):
    //   new = limit_at_grab - (y - grab_y) * 78.0 / control_height
    // (0x56de10 and the 0x57c770 height field, 234 px), clamped -40..+12.
    // The rebuilt bar draws the same span, so one pixel is
    // span_db / bar.height() dB rather than the 1 dB/px of the separate
    // 60 px limiter controls 0x96-0x98.
    let grab_id = id.with("limit-grab");
    if enabled && resp.drag_started() {
        let y = ui.input(|i| {
            i.pointer
                .press_origin()
                .or_else(|| i.pointer.interact_pos())
                .map(|p| p.y)
                .unwrap_or(0.0)
        });
        ui.ctx().data_mut(|d| d.insert_temp(grab_id, (*limit, y)));
    }
    if enabled && resp.dragged() {
        if let Some((limit_at_grab, grab_y)) =
            ui.ctx().data(|d| d.get_temp::<(f32, f32)>(grab_id))
        {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let db_per_px = span_db / bar.height().max(1.0);
                *limit = (limit_at_grab - (pos.y - grab_y) * db_per_px)
                    .clamp(LIMIT_MIN, GAIN_MAX);
            }
        }
    }
    if enabled && resp.double_clicked() {
        // FUN_00541510: a double click on a limiter control restores +12 dB
        // (0x56db6c), not 0 dB.
        *limit = GAIN_MAX;
    }
    let shown_db = lin_to_db(meter).max(METER_FLOOR_DB);
    let _ = resp.on_hover_text(format!(
        "Level {shown_db:.1} dB (scale {min_db:.0} to {:.0} dB) - drag to set the brickwall limit",
        min_db + span_db
    ));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Rew,
    Ff,
    Play,
    Stop,
    Rec,
}

fn transport_button(
    ui: &egui::Ui,
    rect: Rect,
    t: Transport,
    active: bool,
    color: Color32,
    enabled: bool,
) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, Id::new(("transport", t as u8)), sense);
    let hovered = resp.hovered() && enabled;
    let fill = if active {
        color.gamma_multiply(0.85)
    } else if hovered {
        PANEL_HI
    } else {
        Color32::from_rgb(30, 34, 41)
    };
    let p = ui.painter();
    p.rect_filled(rect, 3.0, fill);
    p.rect_stroke(rect, 3.0, Stroke::new(1.0, if active { color } else { LINE }));
    let c = rect.center();
    let col = if active {
        Color32::from_rgb(16, 17, 20)
    } else if enabled {
        TEXT
    } else {
        FAINT
    };
    match t {
        Transport::Play => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    c + Vec2::new(-4.0, -6.0),
                    c + Vec2::new(6.0, 0.0),
                    c + Vec2::new(-4.0, 6.0),
                ],
                col,
                Stroke::NONE,
            ));
        }
        Transport::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(9.0)), 1.0, col);
        }
        Transport::Rec => {
            p.circle_filled(c - Vec2::new(6.0, 0.0), 4.5, RED);
            p.text(
                c + Vec2::new(6.0, 0.0),
                Align2::CENTER_CENTER,
                "REC",
                FontId::proportional(7.0),
                col,
            );
        }
        Transport::Rew => {
            for dx in [-5.0f32, 1.0] {
                p.add(egui::Shape::convex_polygon(
                    vec![
                        c + Vec2::new(dx + 4.0, -5.0),
                        c + Vec2::new(dx - 4.0, 0.0),
                        c + Vec2::new(dx + 4.0, 5.0),
                    ],
                    col,
                    Stroke::NONE,
                ));
            }
        }
        Transport::Ff => {
            for dx in [-1.0f32, 5.0] {
                p.add(egui::Shape::convex_polygon(
                    vec![
                        c + Vec2::new(dx + 4.0, -5.0),
                        c + Vec2::new(dx - 4.0, 0.0),
                        c + Vec2::new(dx + 4.0, 5.0),
                    ],
                    col,
                    Stroke::NONE,
                ));
            }
        }
    }
    resp.clicked() && enabled
}

/// "Microphone (Virtual Cable 03)" -> "Virtual Cable 03", the part of a USB
/// cable endpoint name the narrow strip header can actually show.
/// True when the kernel driver's cable endpoints are already present. The
/// driver exposes four speaker endpoints named "Speakers (Virtual Cable NN)";
/// the USB/IP cables only exist while the mixer's own server runs, so seeing
/// them before the server starts means the driver is installed.
fn driver_cables_present() -> bool {
    driver_cables_present_from(&AudioEngine::output_devices())
}

/// Same test against a list that was produced by the background probe. The
/// enumerated list must never be taken on the UI thread: when a cable endpoint
/// is wedged `EnumAudioEndpoints` blocks for minutes and the whole mixer stops
/// responding.
fn driver_cables_present_from(devices: &[String]) -> bool {
    let mut labels: Vec<String> = devices
        .iter()
        .filter(|d| d.to_ascii_lowercase().starts_with("speakers"))
        .filter_map(|d| cable_label(d))
        // Only the kernel driver makes "Virtual Cable" endpoints. The USB/IP
        // set is called "USB Cable" and must not be read as the driver: with
        // four USB cables attached this used to report KERNEL DRIVER ACTIVE and
        // an automatic B1/B2 then looked for a "Virtual Cable" that is not
        // there.
        .filter(|label| label.starts_with("Virtual Cable "))
        .collect();
    labels.sort();
    labels.dedup();
    labels.len() >= 4
}

fn cable_render_device(devices: &[String], label: &str) -> String {
    devices
        .iter()
        .find(|d| {
            d.to_ascii_lowercase().starts_with("speakers")
                && cable_label(d).as_deref() == Some(label)
        })
        .cloned()
        .unwrap_or_default()
}

fn cable_label(device: &str) -> Option<String> {
    let lower = device.to_ascii_lowercase();
    for (prefix, display) in [
        ("virtual cable ", "Virtual Cable "),
        ("usb cable ", "USB Cable "),
    ] {
        let Some(at) = lower.find(prefix) else {
            continue;
        };
        let digits: String = lower[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(number) = digits.parse::<u32>() {
            return Some(format!("{display}{number:02}"));
        }
    }
    None
}

/// True for the emulated USB/IP cable endpoints (the kernel driver's
/// "Virtual Cable" endpoints are ordinary WDM devices that exist from boot).
fn is_usb_cable_endpoint(device: &str) -> bool {
    cable_label(device)
        .map(|label| label.starts_with("USB Cable"))
        .unwrap_or(false)
}

/// Cable number behind an endpoint name ("Speakers (USB Cable 02)" -> 2), for
/// both the USB/IP cables and the kernel driver's cables.
fn cable_number(device: &str) -> Option<u32> {
    cable_label(device).and_then(|label| {
        label
            .rsplit_once(' ')
            .and_then(|(_, digits)| digits.parse::<u32>().ok())
    })
}

/// Bus index -> strips that would hear their own output. Pure form of
/// `MixerApp::feedback_loops`, so it can be checked without a window.
fn feedback_loops_from(
    bus_cables: &[Option<u32>; audio::MAX_BUSES],
    strip_cables: &[Option<u32>],
    routes: &[[bool; audio::MAX_BUSES]],
) -> [Vec<usize>; audio::MAX_BUSES] {
    let mut loops: [Vec<usize>; audio::MAX_BUSES] = std::array::from_fn(|_| Vec::new());
    for bus in 0..audio::MAX_BUSES {
        let Some(cable) = bus_cables[bus] else {
            continue;
        };
        for (strip, strip_cable) in strip_cables.iter().enumerate() {
            if *strip_cable != Some(cable) {
                continue;
            }
            let feeds_cable = routes.get(strip).is_some_and(|route| {
                (0..audio::MAX_BUSES).any(|b| route[b] && bus_cables[b] == Some(cable))
            });
            if feeds_cable {
                loops[bus].push(strip);
            }
        }
    }
    loops
}

fn clock(secs: f32) -> String {
    let s = secs.max(0.0) as u32;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn fit_text(painter: &egui::Painter, text: &str, size: f32, max_w: f32) -> String {
    let font = FontId::proportional(size);
    if painter
        .layout_no_wrap(text.to_string(), font.clone(), TEXT)
        .size()
        .x
        <= max_w
    {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut trial = out.clone();
        trial.push(ch);
        trial.push_str("..");
        if painter
            .layout_no_wrap(trial.clone(), font.clone(), TEXT)
            .size()
            .x
            > max_w
        {
            break;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "..".to_string()
    } else {
        out.push_str("..");
        out
    }
}

fn save_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let mut bytes = Vec::with_capacity(image.width() * image.height() * 4);
    for p in &image.pixels {
        bytes.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
    }
    image::save_buffer_with_format(
        path,
        &bytes,
        image.width() as u32,
        image.height() as u32,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Window/taskbar icon, decoded from the same .ico that `app.rc` embeds in the
/// executable so the title bar and Explorer never disagree.
fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../icons/Icon_2_48_48_1.ico");
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico)
        .ok()?
        .to_rgba8();
    Some(egui::IconData {
        rgba: image.as_raw().clone(),
        width: image.width(),
        height: image.height(),
    })
}

/// Window size and position stored by the previous run, if any. Read before
/// the viewport is built so the console opens where the person left it.
fn load_saved_window() -> (Option<[f32; 2]>, Option<[f32; 2]>, bool, Option<f32>) {
    let Some(text) = std::fs::read_to_string(settings_canonical_path()).ok() else {
        return (None, None, false, None);
    };
    let Some((attrs, _)) = xml_elements(&text, "Window").into_iter().next() else {
        return (None, None, false, None);
    };

    let width = xml_attr(&attrs, "width").and_then(|v| v.parse::<f32>().ok());
    let height = xml_attr(&attrs, "height").and_then(|v| v.parse::<f32>().ok());
    let size = match (width, height) {
        (Some(w), Some(h)) if (400.0..=8000.0).contains(&w) && (300.0..=8000.0).contains(&h) => {
            Some([w, h])
        }
        _ => None,
    };

    let x = xml_attr(&attrs, "x").and_then(|v| v.parse::<f32>().ok());
    let y = xml_attr(&attrs, "y").and_then(|v| v.parse::<f32>().ok());
    let pos = match (x, y) {
        (Some(x), Some(y))
            if (-32000.0..=32000.0).contains(&x) && (-32000.0..=32000.0).contains(&y) =>
        {
            Some([x, y])
        }
        _ => None,
    };

    let maximized = xml_attr(&attrs, "maximized")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let zoom = xml_attr(&attrs, "zoom")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|factor| (0.5..=3.0).contains(factor));

    (size, pos, maximized, zoom)
}

/// Sidecar diagnostics. The console is a GUI-subsystem binary, so a panic or a
/// hang would otherwise leave no trace at all.
fn diag_path(name: &str) -> PathBuf {
    settings_canonical_path().with_file_name(name)
}

fn diag_log(message: impl AsRef<str>) {
    use std::io::Write;
    let path = diag_path("startup.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 512 * 1024 {
            let _ = std::fs::remove_file(&path);
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "[{stamp}] {}", message.as_ref());
    }
}

fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let path = diag_path("panic.log");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{info}");
        }
    }));
}

fn main() -> eframe::Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    install_panic_log();
    diag_log("main: entry");
    // Child-process VST3 scanner (see vst::start_scan). It only reads plug-in
    // metadata and writes a report, so a bad plug-in cannot hurt the console.
    if let Some(position) = raw_args.iter().position(|arg| arg == "--vst-scan-json") {
        if let Some(path) = raw_args.get(position + 1) {
            if let Err(err) = vst::write_scan_report(Path::new(path)) {
                eprintln!("VST scan helper: {err}");
            }
        }
        return Ok(());
    }
    if let Some(position) = raw_args.iter().position(|arg| arg == "--vc-attach-service") {
        let port = raw_args
            .get(position + 1)
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(3241);
        let token = raw_args.get(position + 2).cloned().unwrap_or_default();
        let log = vc_file_logger();
        if let Err(err) = virtual_cables::win::run_attach_service(port, &token, &log) {
            log(format!("attach helper failed: {err}"));
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(position) = raw_args.iter().position(|arg| arg == "--vc-attach-broker") {
        let address = raw_args.get(position + 1).cloned().unwrap_or_default();
        let token = raw_args.get(position + 2).cloned().unwrap_or_default();
        let log: virtual_cables::LogFn =
            std::sync::Arc::new(|line: String| eprintln!("{line}"));
        if let Err(err) = virtual_cables::win::run_attach_broker(&address, &token, &log) {
            eprintln!("virtual cables attach broker failed: {err}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--vc-install-helper") {
        // Refresh the resident helper: it now runs from its own copy so it never
        // holds audio_mixer_pro.exe open.
        let log: virtual_cables::LogFn =
            std::sync::Arc::new(|line: String| eprintln!("{line}"));
        match virtual_cables::win::install_helper(&log) {
            Ok(()) => println!("helper install started"),
            Err(err) => println!("helper install failed: {err}"),
        }
        // Give the elevated script time to copy and re-register the task.
        std::thread::sleep(std::time::Duration::from_secs(8));
        if let Some(path) = virtual_cables::win::helper_exe_path() {
            println!("helper copy: {} (exists={})", path.display(), path.exists());
        }
        println!("helper running: {}", virtual_cables::win::helper_running());
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--eq-selftest") {
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("eq_selftest.txt");
        audio::strip_eq_selftest(&path);
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--dynamics-selftest") {
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("dynamics_selftest.txt");
        audio::dynamics_selftest(&path);
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--ducking-selftest") {
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("ducking_selftest.txt");
        audio::ducking_selftest(&path);
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--apps-selftest") {
        let report = sessions::self_test();
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("apps_selftest.txt");
        if let Err(err) = std::fs::write(&path, &report) {
            eprintln!("could not write apps_selftest.txt: {err}");
        }
        return Ok(());
    }
    if raw_args.iter().any(|arg| arg == "--vc-self-test") {
        let config = virtual_cables::Config::import_legacy()
            .unwrap_or_default()
            .normalized();
        match virtual_cables::self_test(&config) {
            Ok(report) => eprint!("{report}"),
            Err(err) => {
                eprintln!("virtual cables self-test failed: {err}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let mut shot_path = None;
    let mut audio_selftest = false;
    let mut audio_wav_selftest = false;
    let mut audio_routing_selftest = false;
    let mut meter_selftest_flag = false;
    let mut startup_demo = None;
    let mut vst_probe: Option<Option<PathBuf>> = None;
    let mut vst_selftest: Option<Option<PathBuf>> = None;
    let mut clap_selftest: Option<Option<PathBuf>> = None;
    let mut clap_gui: Option<Option<PathBuf>> = None;
    let mut plugin_selftest: Option<PathBuf> = None;
    let mut vst_mix_selftest: Option<Option<PathBuf>> = None;
    let mut vst_multi_selftest: Option<Option<PathBuf>> = None;
    let mut vst_editor_selftest: Option<Option<PathBuf>> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--screenshot" {
            shot_path = args.next().map(PathBuf::from);
        } else if arg == "--vst-probe" {
            // Optional plug-in path; a following option is not a path.
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            vst_probe = Some(path);
        } else if arg == "--vst-selftest" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            vst_selftest = Some(path);
        } else if arg == "--clap-selftest" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            clap_selftest = Some(path);
        } else if arg == "--plugin-selftest" {
            plugin_selftest = args.next().map(PathBuf::from);
        } else if arg == "--clap-gui" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            clap_gui = Some(path);
        } else if arg == "--vst-mix-selftest" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            vst_mix_selftest = Some(path);
        } else if arg == "--vst-multi-selftest" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            vst_multi_selftest = Some(path);
        } else if arg == "--vst-editor-selftest" {
            let next = args.next();
            let path = next.and_then(|value| {
                if value.starts_with('-') {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            });
            vst_editor_selftest = Some(path);
        } else if arg == "--audio-selftest" {
            audio_selftest = true;
        } else if arg == "--audio-selftest-wav" {
            audio_wav_selftest = true;
        } else if arg == "--routing-selftest" {
            audio_routing_selftest = true;
        } else if arg == "--meter-selftest" {
            meter_selftest_flag = true;
        } else if arg == "--input-selftest" {
            audio::input_selftest();
            return Ok(());
        } else if arg == "--virtual-bus-selftest" {
            audio::virtual_bus_selftest();
            return Ok(());
        } else if arg == "--vban-selftest" {
            vban::vban_selftest();
            return Ok(());
        } else if arg == "--cable-peak" {
            let name = args.next();
            let seconds = args.next().and_then(|v| v.parse::<f32>().ok()).unwrap_or(2.0);
            audio::cable_peak(name.as_deref(), seconds);
            return Ok(());
        } else if arg == "--play-demo" {
            startup_demo = args.next().and_then(|v| v.parse::<usize>().ok());
        }
    }
    if raw_args.iter().any(|arg| arg == "--list-devices") {
        let outputs = AudioEngine::output_devices();
        let inputs = InputCapture::devices();
        println!("outputs ({}):", outputs.len());
        for name in &outputs {
            println!("  {name}");
        }
        println!("inputs ({}):", inputs.len());
        for name in &inputs {
            println!("  {name}");
        }
        return Ok(());
    }
    if audio_selftest {
        audio::selftest();
        return Ok(());
    }
    if audio_wav_selftest {
        audio::wav_selftest();
        return Ok(());
    }
    if audio_routing_selftest {
        audio::routing_selftest();
        return Ok(());
    }
    if meter_selftest_flag {
        meter_selftest();
        return Ok(());
    }
    if let Some(path) = vst_probe {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("vst_probe.txt");
        vst::probe(path.as_deref(), &report);
        return Ok(());
    }
    if let Some(path) = vst_selftest {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("vst_selftest.txt");
        vst::selftest(path.as_deref(), &report);
        return Ok(());
    }
    if let Some(path) = plugin_selftest {
        let mut lines = Vec::new();
        let slot = plugin::PlugIn::new();
        match slot.load(&path, 48_000.0, vst::VST_MAX_BLOCK) {
            Ok(info) => {
                lines.push(format!("loaded: {} [{}]", info.name, info.format));
                let frames = 512usize;
                let mut in_l = vec![0.0f32; frames];
                let mut in_r = vec![0.0f32; frames];
                let mut out_l = vec![0.0f32; frames];
                let mut out_r = vec![0.0f32; frames];
                let (mut processed, mut peak, mut nonfinite) = (0usize, 0.0f32, 0usize);
                for block in 0..96 {
                    for i in 0..frames {
                        let t = (block * frames + i) as f32 / 48_000.0;
                        let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
                        in_l[i] = v;
                        in_r[i] = v;
                    }
                    if slot.process(&in_l, &in_r, &mut out_l, &mut out_r) {
                        processed += 1;
                        for i in 0..frames {
                            if !out_l[i].is_finite() || !out_r[i].is_finite() {
                                nonfinite += 1;
                            }
                            peak = peak.max(out_l[i].abs()).max(out_r[i].abs());
                        }
                    }
                }
                lines.push(format!(
                    "process: blocks={processed}/96 peak={peak:.6} nonfinite={nonfinite}"
                ));
                lines.push(format!("latency: {:?}", slot.latency()));
                let params = slot.parameters();
                lines.push(format!("params: {}", params.len()));
                // Prove a CLAP parameter change actually lands: mute the first
                // parameter that formats as a percentage and watch the peak.
                if let Some(param) = params.iter().find(|p| p.formatted.contains("ms")) {
                    let before = slot.parameters().iter().find(|p| p.id == param.id).map(|p| p.value);
                    let _ = slot.set_parameter(param.id, 0.9);
                    for block in 0..4 {
                        for i in 0..frames {
                            let t = (block * frames + i) as f32 / 48_000.0;
                            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
                            in_l[i] = v;
                            in_r[i] = v;
                        }
                        slot.process(&in_l, &in_r, &mut out_l, &mut out_r);
                    }
                    let after = slot.parameters().iter().find(|p| p.id == param.id).map(|p| p.value);
                    lines.push(format!(
                        "param event: '{}' {} -> {:?}",
                        param.name, before.map(|v| format!("{v:.3}")).unwrap_or_default(), after
                    ));
                }
                if let Some(err) = slot.error() {
                    lines.push(format!("error: {err}"));
                }
            }
            Err(err) => lines.push(format!("FAIL: {err}")),
        }
        let text = lines.join("
") + "
";
        print!("{text}");
        return Ok(());
    }
    if let Some(path) = clap_gui {
        let chosen = path.or_else(|| {
            clap_slot::scan()
                .into_iter()
                .find(|entry| entry.category().contains("audio-effect"))
                .map(|entry| entry.path)
        });
        let outcome = match chosen {
            Some(path) => clap_slot::gui_selftest(&path),
            None => clap_slot::SelftestReport {
                ok: false,
                lines: vec!["no CLAP plug-in found".to_string()],
            },
        };
        let text = outcome.lines.join("
") + "
";
        let _ = std::fs::write(
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("clap_gui.txt"),
            &text,
        );
        print!("{text}");
        return Ok(());
    }
    if let Some(path) = clap_selftest {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("clap_selftest.txt");
        let outcome = match path {
            Some(path) => clap_slot::selftest(&path),
            None => {
                let found = clap_slot::scan();
                let mut lines = vec![format!("scanned {} CLAP plug-in(s)", found.len())];
                for entry in &found {
                    lines.push(format!(
                        "  {} | {} | {} | {} | {}",
                        entry.name,
                        entry.vendor,
                        entry.version,
                        entry.category(),
                        entry.path.display()
                    ));
                }
                match found.first() {
                    Some(first) => {
                        let mut result = clap_slot::selftest(&first.path);
                        lines.push(String::new());
                        lines.push(format!("selftest: {}", first.name));
                        lines.extend(result.lines.drain(..));
                        clap_slot::SelftestReport {
                            ok: result.ok,
                            lines,
                        }
                    }
                    None => clap_slot::SelftestReport {
                        ok: false,
                        lines,
                    },
                }
            }
        };
        let text = outcome.lines.join("
") + "
";
        let _ = std::fs::write(&report, &text);
        print!("{text}");
        return Ok(());
    }
    if let Some(path) = vst_mix_selftest {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("vst_mix_selftest.txt");
        let plugin = path.unwrap_or_else(|| {
            PathBuf::from(r"C:\Program Files\Common Files\VST3\7Q.vst3")
        });
        audio::vst_mix_selftest(&plugin, &report);
        return Ok(());
    }
    if let Some(path) = vst_multi_selftest {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("vst_multi_selftest.txt");
        let plugin = path.unwrap_or_else(|| {
            PathBuf::from(r"C:\Program Files\Common Files\VST3\7Q.vst3")
        });
        audio::vst_multichannel_selftest(&plugin, &report);
        return Ok(());
    }
    if let Some(path) = vst_editor_selftest {
        let report = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("vst_editor_selftest.txt");
        vst::editor_selftest(path.as_deref(), &report);
        return Ok(());
    }

    // UI zoom. The console is a fixed-size window, so on a small or scaled
    // display the only practical knob is the zoom factor.
    let arg_zoom = raw_args
        .iter()
        .position(|arg| arg == "--zoom")
        .and_then(|index| raw_args.get(index + 1))
        .and_then(|value| value.parse::<f32>().ok())
        .or_else(|| {
            std::env::var("AMP_ZOOM")
                .ok()
                .and_then(|value| value.parse::<f32>().ok())
        })
        .filter(|factor| (0.5..=3.0).contains(factor));

    // Reopen at the size, position and zoom the user left, when a previous
    // run saved them. An explicit --zoom still wins.
    let (saved_size, saved_pos, saved_maximized, saved_zoom) = load_saved_window();
    let zoom = arg_zoom.or(saved_zoom).unwrap_or(1.0);
    diag_log(format!("main: ui zoom {zoom}"));
    let min_size = [860.0 * zoom, 540.0 * zoom];
    let initial_size = saved_size
        .map(|[w, h]| [w.max(min_size[0]), h.max(min_size[1])])
        .unwrap_or([1024.0 * zoom, 620.0 * zoom]);
    let initial_pos = saved_pos.unwrap_or([40.0, 40.0]);
    diag_log(format!(
        "main: window {}x{} at {},{} maximized={}",
        initial_size[0], initial_size[1], initial_pos[0], initial_pos[1], saved_maximized
    ));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(initial_size)
        .with_min_inner_size(min_size)
        // An undecorated window can come up minimized when the desktop size
        // changed; ask for focus and open where it was last left.
        .with_position(egui::pos2(initial_pos[0], initial_pos[1]))
        .with_visible(true)
        .with_active(true)
        // Borderless, but resizable from the corner grip (and the top-bar
        // maximize button); the layout reflows with the window.
        .with_resizable(true)
        .with_decorations(false)
        .with_transparent(false)
        .with_title("Audio Mixer Pro");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    diag_log("main: creating window");
    let result = eframe::run_native(
        "Audio Mixer Pro",
        options,
        Box::new(move |cc| {
            if (zoom - 1.0).abs() > f32::EPSILON {
                cc.egui_ctx.set_zoom_factor(zoom);
            }
            let mut app = MixerApp::new(cc, shot_path.clone(), startup_demo);
            app.pending_maximize = saved_maximized;
            Ok(Box::new(app))
        }),
    );
    if let Err(err) = &result {
        // A GUI-subsystem binary prints nothing, and a missing OpenGL context
        // (for example a VM without 3D acceleration) fails exactly here.
        diag_log(format!("main: window creation failed: {err}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_free_cable_skips_bus_and_used_cables() {
        let inputs = vec![
            "Microphone (Virtual Cable 01)".to_string(),
            "Microphone (Virtual Cable 02)".to_string(),
            "Microphone (Virtual Cable 03)".to_string(),
            "Microphone (Virtual Cable 04)".to_string(),
            "CABLE Output (VB-Audio Virtual Cable)".to_string(),
        ];
        let used = vec![
            "microphone (virtual cable 03)".to_string(),
            "virtual 1 (vaio)".to_string(),
        ];
        assert_eq!(
            MixerApp::next_free_virtual_cable_from(&inputs, &used).as_deref(),
            Some("Microphone (Virtual Cable 01)")
        );
    }

    #[test]
    fn usb_cables_do_not_count_as_the_kernel_driver() {
        let devices: Vec<String> = (1..=4)
            .map(|n| format!("Speakers (USB Cable {n:02})"))
            .collect();
        assert!(!driver_cables_present_from(&devices));
    }

    #[test]
    fn four_driver_cables_count_as_the_kernel_driver() {
        let mut devices: Vec<String> = (1..=4)
            .map(|n| format!("Speakers (Virtual Cable {n:02})"))
            .collect();
        // Both sets installed at once: the USB set must not change the answer.
        devices.extend((1..=4).map(|n| format!("Speakers (USB Cable {n:02})")));
        assert!(driver_cables_present_from(&devices));
    }

    #[test]
    fn feedback_loop_flags_every_bus_on_the_cable() {
        // B1 and B3 both play into cable 01; B2 into cable 02.
        let bus_cables: [Option<u32>; audio::MAX_BUSES] =
            [None, None, None, Some(1), Some(2), Some(1)];
        // INPUT 1 listens to cable 01 and is routed to B3 only.
        let strip_cables = [Some(1)];
        let mut route = [false; audio::MAX_BUSES];
        route[5] = true;
        let loops = feedback_loops_from(&bus_cables, &strip_cables, &[route]);
        // Both buses on that cable feed the strip, so both are flagged.
        assert_eq!(loops[3], vec![0]);
        assert_eq!(loops[5], vec![0]);
        assert!(loops[4].is_empty());
    }

    #[test]
    fn strip_on_one_cable_routed_to_another_is_not_a_loop() {
        let bus_cables: [Option<u32>; audio::MAX_BUSES] = [None, None, None, Some(1), Some(2), None];
        let strip_cables = [Some(1)];
        let mut route = [false; audio::MAX_BUSES];
        route[4] = true; // B2 plays into cable 02
        let loops = feedback_loops_from(&bus_cables, &strip_cables, &[route]);
        assert!(loops.iter().all(|bus| bus.is_empty()));
    }

    #[test]
    fn next_free_cable_none_when_all_used() {
        let inputs = vec!["Microphone (Virtual Cable 03)".to_string()];
        let used = vec!["microphone (virtual cable 03)".to_string()];
        assert!(MixerApp::next_free_virtual_cable_from(&inputs, &used).is_none());
    }
}

fn vc_file_logger() -> virtual_cables::LogFn {
    let path = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("Virtual Cables").join("attach_helper.log"));
    std::sync::Arc::new(move |line: String| {
        if let Some(path) = &path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
            }
        }
    })
}
