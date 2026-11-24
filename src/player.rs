//! Internal streaming player.
//!
//! Decodes a playlist on a worker thread and pushes stereo frames into the same
//! ring an input capture uses, so a strip reads the player exactly like a cable:
//! resampler, dynamics, VST slot, ASIO insert and bus routing all apply with no
//! special case in the mix loop.
//!
//! Nothing here runs on the audio thread. The callback only pops frames from the
//! ring, which is what keeps a long file and a slow disk from reaching the mix.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::audio::InputShared;

const PUSH_BLOCK: usize = 1024;
/// How far ahead of the player the strip ring is fed. The input reader holds its
/// own target at 20 ms and starts slipping frames above 40 ms, so the player
/// keeps the ring under that and holds its real cushion in its own queue.
const RING_TARGET_MS: u32 = 30;
/// Decoded audio the worker keeps queued in front of the ring. A slow disk or a
/// CPU spike is absorbed here; the strip never sees it.
const DECODE_AHEAD_MS: usize = 750;
const IDLE_SLEEP_MS: u64 = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeat {
    Off,
    One,
    All,
}

impl Repeat {
    pub fn to_u8(self) -> u8 {
        match self {
            Repeat::Off => 0,
            Repeat::One => 1,
            Repeat::All => 2,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Repeat::One,
            2 => Repeat::All,
            _ => Repeat::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "OFF",
            Repeat::One => "ONE",
            Repeat::All => "ALL",
        }
    }
}

/// One entry of the playlist. `duration_ms` comes from the playlist file when
/// it has one, so the panel can show lengths without opening every track.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub duration_ms: Option<u64>,
}

impl Track {
    pub fn new(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            path,
            title,
            duration_ms: None,
        }
    }

    pub fn with_duration(mut self, ms: Option<u64>) -> Self {
        if ms.unwrap_or(0) > 0 {
            self.duration_ms = ms;
        }
        self
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }
}

struct Shared {
    tracks: Mutex<Vec<Track>>,
    index: AtomicUsize,
    playing: AtomicBool,
    position_ms: AtomicU64,
    duration_ms: AtomicU64,
    /// u64::MAX = none pending.
    seek_ms: AtomicU64,
    next_req: AtomicBool,
    prev_req: AtomicBool,
    stop_req: AtomicBool,
    crossfade_ms: AtomicU32,
    repeat: AtomicU8,
    shuffle: AtomicBool,
    /// Bumped whenever the playlist or the current index changes, so the worker
    /// knows to rebuild its decoders.
    generation: AtomicU64,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
    /// Set when the worker has a track open and is feeding the ring.
    active: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            tracks: Mutex::new(Vec::new()),
            index: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
            position_ms: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            seek_ms: AtomicU64::new(u64::MAX),
            next_req: AtomicBool::new(false),
            prev_req: AtomicBool::new(false),
            stop_req: AtomicBool::new(false),
            crossfade_ms: AtomicU32::new(3000),
            repeat: AtomicU8::new(Repeat::Off.to_u8()),
            shuffle: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            active: AtomicBool::new(false),
        }
    }

    fn current_track(&self) -> Option<Track> {
        let tracks = lock(&self.tracks);
        tracks.get(self.index.load(Ordering::Relaxed)).cloned()
    }
}

/// Cheap mutex helper: a poisoned lock must never take the mixer down.
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One streaming decoder. Owns the format reader and the codec, and hands out
/// interleaved stereo frames without ever loading the whole file.
struct StreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    rate: u32,
    channels: usize,
    pending: Vec<[f32; 2]>,
    pending_pos: usize,
    pos_frames: u64,
    total_frames: Option<u64>,
    ended: bool,
}

impl StreamDecoder {
    fn open(path: &Path) -> Result<Self, String> {
        let file =
            std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(&ext.to_ascii_lowercase());
        }
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("unrecognised audio container: {e}"))?;
        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| "the file has no audio track".to_string())?;
        let track_id = track.id;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("no decoder for this audio: {e}"))?;
        let rate = params.sample_rate.unwrap_or(48_000).max(8_000);
        let channels = params.channels.map(|c| c.count()).unwrap_or(2).max(1);
        let total_frames = params.n_frames;
        Ok(Self {
            format,
            decoder,
            track_id,
            rate,
            channels,
            pending: Vec::new(),
            pending_pos: 0,
            pos_frames: 0,
            total_frames,
            ended: false,
        })
    }

    fn rate(&self) -> u32 {
        self.rate
    }

    fn remaining_frames(&self) -> Option<u64> {
        self.total_frames
            .map(|total| total.saturating_sub(self.pos_frames))
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), String> {
        let time = Time {
            seconds: ms / 1000,
            frac: (ms % 1000) as f64 / 1000.0,
        };
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("could not seek: {e}"))?;
        self.pending.clear();
        self.pending_pos = 0;
        self.pos_frames = (ms as f64 * self.rate as f64 / 1000.0).round() as u64;
        self.ended = false;
        Ok(())
    }

    /// Next stereo frame, decoding another packet when the last one is used up.
    fn next_frame(&mut self) -> Option<[f32; 2]> {
        loop {
            if self.pending_pos < self.pending.len() {
                let frame = self.pending[self.pending_pos];
                self.pending_pos += 1;
                self.pos_frames += 1;
                return Some(frame);
            }
            if self.ended {
                return None;
            }
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.ended = true;
                    return None;
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.ended = true;
                    return None;
                }
                Err(_) => {
                    self.ended = true;
                    return None;
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(_) => {
                    self.ended = true;
                    return None;
                }
            };
            let spec = *decoded.spec();
            self.rate = spec.rate.max(8_000);
            self.channels = spec.channels.count().max(1);
            let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();
            let channels = self.channels.max(1);
            self.pending.clear();
            self.pending_pos = 0;
            for frame in samples.chunks(channels) {
                let l = frame.first().copied().unwrap_or(0.0);
                let r = if channels >= 2 {
                    frame.get(1).copied().unwrap_or(l)
                } else {
                    l
                };
                self.pending.push([
                    if l.is_finite() { l.clamp(-1.5, 1.5) } else { 0.0 },
                    if r.is_finite() { r.clamp(-1.5, 1.5) } else { 0.0 },
                ]);
            }
            if self.pending.is_empty() {
                continue;
            }
        }
    }
}

/// The active program: the track that is playing plus, while a crossfade runs,
/// the one fading in.
struct Program {
    cur: StreamDecoder,
    cur_index: usize,
    next: Option<StreamDecoder>,
    next_index: usize,
    fade_len: usize,
    fade_pos: usize,
    /// Set once the current track ran out with nothing queued.
    ended: bool,
}

impl Program {
    fn new(cur: StreamDecoder, cur_index: usize) -> Self {
        Self {
            cur,
            cur_index,
            next: None,
            next_index: cur_index,
            fade_len: 0,
            fade_pos: 0,
            ended: false,
        }
    }

    fn fading(&self) -> bool {
        self.next.is_some()
    }

    fn begin_fade(
        &mut self,
        next: StreamDecoder,
        next_index: usize,
        fade_frames: usize,
    ) {
        self.next = Some(next);
        self.next_index = next_index;
        self.fade_len = fade_frames.max(1);
        self.fade_pos = 0;
    }

    fn promote(&mut self) {
        if let Some(next) = self.next.take() {
            self.cur = next;
            self.cur_index = self.next_index;
        }
        self.fade_len = 0;
        self.fade_pos = 0;
    }

    /// Fill `out` with program frames. Returns true when the program ended.
    fn next_block(&mut self, out: &mut Vec<[f32; 2]>, want: usize) -> bool {
        out.clear();
        out.reserve(want);
        while out.len() < want {
            let Some(cur) = self.cur.next_frame() else {
                if self.fading() {
                    self.promote();
                    continue;
                }
                self.ended = true;
                return true;
            };
            let mut frame = cur;
            if self.fading() {
                let next_frame = self.next.as_mut().and_then(|d| d.next_frame());
                if let Some(nf) = next_frame {
                    let t = (self.fade_pos as f32 + 0.5) / self.fade_len.max(1) as f32;
                    let angle = t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
                    let (sin, cos) = angle.sin_cos();
                    frame = [
                        frame[0] * cos + nf[0] * sin,
                        frame[1] * cos + nf[1] * sin,
                    ];
                    self.fade_pos += 1;
                    if self.fade_pos >= self.fade_len {
                        self.promote();
                    }
                } else {
                    // The outgoing decoder ran dry first; finish the crossfade
                    // immediately rather than leaving silence.
                    self.promote();
                }
            }
            out.push(frame);
        }
        false
    }
}

pub struct Player {
    shared: Arc<Shared>,
    input: Arc<InputShared>,
    /// Extra strip rings fed with the same decoded audio (demo/mirror mode).
    targets: Arc<Mutex<Vec<Arc<InputShared>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    pub fn new(input: Arc<InputShared>) -> Self {
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let targets: Arc<Mutex<Vec<Arc<InputShared>>>> = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let shared = Arc::clone(&shared);
            let input = Arc::clone(&input);
            let stop = Arc::clone(&stop);
            let targets = Arc::clone(&targets);
            std::thread::Builder::new()
                .name("mixer-player".to_string())
                .spawn(move || worker(shared, input, targets, stop))
                .ok()
        };
        Self {
            shared,
            input,
            targets,
            stop,
            thread,
        }
    }

    pub fn input(&self) -> Arc<InputShared> {
        Arc::clone(&self.input)
    }

    /// Feed the same decoded audio into these extra strip rings as well.
    /// Each ring is consumed by its own strip reader, so every strip sees the
    /// identical sample stream at the same moment.
    pub fn set_targets(&self, targets: Vec<Arc<InputShared>>) {
        *lock(&self.targets) = targets;
    }

    // --- transport -------------------------------------------------------

    pub fn play(&self) {
        if self.shared.current_track().is_none() {
            return;
        }
        self.shared.finished.store(false, Ordering::Relaxed);
        self.shared.error.lock().map(|mut e| *e = None).ok();
        self.shared.playing.store(true, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        self.shared.stop_req.store(true, Ordering::Relaxed);
        self.shared.finished.store(false, Ordering::Relaxed);
    }

    pub fn next(&self) {
        self.shared.next_req.store(true, Ordering::Relaxed);
    }

    pub fn previous(&self) {
        self.shared.prev_req.store(true, Ordering::Relaxed);
    }

    pub fn seek_ms(&self, ms: u64) {
        self.shared.seek_ms.store(ms, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn position_ms(&self) -> u64 {
        self.shared.position_ms.load(Ordering::Relaxed)
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms.load(Ordering::Relaxed)
    }

    pub fn index(&self) -> usize {
        self.shared.index.load(Ordering::Relaxed)
    }

    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|e| e.clone())
    }

    pub fn active(&self) -> bool {
        self.shared.active.load(Ordering::Relaxed)
    }

    /// Bumped on every playlist edit, jump or transport change; the UI uses it
    /// to know when its cached copy of the list is stale.
    pub fn revision(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }

    // --- playlist --------------------------------------------------------

    pub fn tracks(&self) -> Vec<Track> {
        lock(&self.shared.tracks).clone()
    }

    pub fn set_tracks(&self, tracks: Vec<Track>, index: usize) {
        let index = index.min(tracks.len().saturating_sub(1));
        *lock(&self.shared.tracks) = tracks;
        self.shared.index.store(index, Ordering::Relaxed);
        self.shared.position_ms.store(0, Ordering::Relaxed);
        self.shared.seek_ms.store(u64::MAX, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
        // Deliberately no autoplay: loading a scene must not start audio.
    }

    pub fn add_tracks(&self, mut tracks: Vec<Track>) {
        let mut list = lock(&self.shared.tracks);
        list.append(&mut tracks);
    }

    pub fn clear(&self) {
        lock(&self.shared.tracks).clear();
        self.shared.index.store(0, Ordering::Relaxed);
        self.stop();
        self.shared.position_ms.store(0, Ordering::Relaxed);
        self.shared.duration_ms.store(0, Ordering::Relaxed);
    }

    /// Jump to `index` and start playing it.
    pub fn jump_to(&self, index: usize) {
        let len = lock(&self.shared.tracks).len();
        if index >= len {
            return;
        }
        self.shared.index.store(index, Ordering::Relaxed);
        self.shared.position_ms.store(0, Ordering::Relaxed);
        self.shared.seek_ms.store(u64::MAX, Ordering::Relaxed);
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
        self.play();
    }

    pub fn remove(&self, index: usize) {
        let mut list = lock(&self.shared.tracks);
        if index >= list.len() {
            return;
        }
        list.remove(index);
        let len = list.len();
        drop(list);
        let current = self.shared.index.load(Ordering::Relaxed);
        if index < current || current >= len {
            self.shared
                .index
                .store(current.saturating_sub(1).min(len.saturating_sub(1)), Ordering::Relaxed);
        }
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn move_track(&self, from: usize, to: usize) {
        let mut list = lock(&self.shared.tracks);
        if from >= list.len() || to >= list.len() || from == to {
            return;
        }
        let track = list.remove(from);
        list.insert(to, track);
        drop(list);
        let current = self.shared.index.load(Ordering::Relaxed);
        if current == from {
            self.shared.index.store(to, Ordering::Relaxed);
        }
        self.shared.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_crossfade_ms(&self, ms: u32) {
        self.shared
            .crossfade_ms
            .store(ms.min(15_000), Ordering::Relaxed);
    }

    pub fn crossfade_ms(&self) -> u32 {
        self.shared.crossfade_ms.load(Ordering::Relaxed)
    }

    pub fn set_repeat(&self, repeat: Repeat) {
        self.shared.repeat.store(repeat.to_u8(), Ordering::Relaxed);
    }

    pub fn repeat(&self) -> Repeat {
        Repeat::from_u8(self.shared.repeat.load(Ordering::Relaxed))
    }

    pub fn set_shuffle(&self, on: bool) {
        self.shared.shuffle.store(on, Ordering::Relaxed);
    }

    pub fn shuffle(&self) -> bool {
        self.shared.shuffle.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn worker(
    shared: Arc<Shared>,
    input: Arc<InputShared>,
    targets: Arc<Mutex<Vec<Arc<InputShared>>>>,
    stop: Arc<AtomicBool>,
) {
    let mut program: Option<Program> = None;
    let mut program_generation = 0u64;
    let mut buf: Vec<[f32; 2]> = Vec::with_capacity(PUSH_BLOCK);
    let mut pending: std::collections::VecDeque<[f32; 2]> =
        std::collections::VecDeque::with_capacity(PUSH_BLOCK * 2);
    /// Playback clock: frames the mixer consumed since this program opened, plus
    /// the position the program was opened at (non-zero after a seek).
    let mut position_base_ms: u64 = 0;
    let mut played_base: u64 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !shared.playing.load(Ordering::Relaxed) {
            program = None;
            pending.clear();
            shared.active.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
            continue;
        }

        // A stopped/finished transport, a fresh jump or a playlist edit all
        // invalidate the open program.
        let generation = shared.generation.load(Ordering::Relaxed);
        if shared.stop_req.swap(false, Ordering::Relaxed) {
            program = None;
            pending.clear();
            shared.active.store(false, Ordering::Relaxed);
            shared.position_ms.store(0, Ordering::Relaxed);
            continue;
        }
        if program_generation != generation {
            program = None;
            pending.clear();
            program_generation = generation;
        }
        if shared.seek_ms.load(Ordering::Relaxed) != u64::MAX {
            // Applied to the current program when possible, otherwise the
            // program is rebuilt from the requested index.
            program = None;
            pending.clear();
            program_generation = generation;
        }

        let index = shared.index.load(Ordering::Relaxed);
        if program.is_none() {
            program = open_program(&shared, index);
            match &program {
                Some(p) => {
                    input.set_rate(p.cur.rate());
                    let duration = duration_for(&shared, index, p);
                    shared.duration_ms.store(duration, Ordering::Relaxed);
                    let seek_target = shared.seek_ms.swap(u64::MAX, Ordering::Relaxed);
                    let start_ms = if seek_target == u64::MAX {
                        0
                    } else {
                        seek_target.min(duration.max(1))
                    };
                    shared.position_ms.store(start_ms, Ordering::Relaxed);
                    position_base_ms = start_ms;
                    played_base = input.played_frames();
                    if start_ms > 0 {
                        if let Some(p) = program.as_mut() {
                            let _ = p.cur.seek_ms(start_ms);
                        }
                    }
                    shared.active.store(true, Ordering::Relaxed);
                }
                None => {
                    shared.active.store(false, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
                    continue;
                }
            }
        }

        let Some(p) = program.as_mut() else {
            continue;
        };

        // Manual skip: start the fade now when crossfade is on.
        if shared.prev_req.swap(false, Ordering::Relaxed) {
            if let Some(new_index) = step_index(&shared, p.cur_index, false) {
                if let Some(decoder) = StreamDecoder::open(&track_path(&shared, new_index)).ok() {
                    let fade = fade_frames(&shared, p.cur.rate());
                    if fade > 0 {
                        p.begin_fade(decoder, new_index, fade);
                    } else {
                        p.cur = decoder;
                        p.cur_index = new_index;
                        shared.index.store(new_index, Ordering::Relaxed);
                    }
                }
            }
        }
        if shared.next_req.swap(false, Ordering::Relaxed) {
            if let Some(new_index) = step_index(&shared, p.cur_index, true) {
                if let Some(decoder) = StreamDecoder::open(&track_path(&shared, new_index)).ok() {
                    let fade = fade_frames(&shared, p.cur.rate());
                    if fade > 0 {
                        p.begin_fade(decoder, new_index, fade);
                    } else {
                        p.cur = decoder;
                        p.cur_index = new_index;
                        shared.index.store(new_index, Ordering::Relaxed);
                    }
                }
            }
        }

        // Schedule an automatic crossfade a little before the track ends.
        if !p.fading() {
            let fade = fade_frames(&shared, p.cur.rate()) as u64;
            if fade > 0 {
                if let Some(remaining) = p.cur.remaining_frames() {
                    if remaining <= fade {
                        if let Some(new_index) = step_index(&shared, p.cur_index, true) {
                            if let Ok(decoder) = StreamDecoder::open(&track_path(&shared, new_index)) {
                                p.begin_fade(decoder, new_index, fade as usize);
                            }
                        }
                    }
                }
            }
        }

        // Decode ahead into this thread's own queue. A disk or CPU hiccup is
        // absorbed here instead of reaching the strip's ring, which only ever
        // holds a few milliseconds.
        let rate_now = p.cur.rate().max(8_000) as usize;
        let ahead = rate_now * DECODE_AHEAD_MS / 1000;
        let mut ended = false;
        if pending.len() < ahead {
            ended = p.next_block(&mut buf, PUSH_BLOCK);
            pending.extend(buf.iter().copied());
        }

        // Feed the ring only while it is below the input reader's high-water
        // mark (it slips frames above 40 ms), so the cushion lives here.
        let ring_target = (rate_now * RING_TARGET_MS as usize / 1000).max(256);
        let mirrors = lock(&targets).clone();
        let fill = mirrors
            .iter()
            .fold(input.ring_fill(), |deepest, target| {
                deepest.max(target.ring_fill())
            });
        if !pending.is_empty() && fill < ring_target {
            let take = pending.len().min(PUSH_BLOCK);
            let mut chunk: Vec<[f32; 2]> = Vec::with_capacity(take);
            for _ in 0..take {
                if let Some(frame) = pending.pop_front() {
                    chunk.push(frame);
                }
            }
            input.set_rate(rate_now as u32);
            input.push_block(&chunk);
            for mirror in &mirrors {
                mirror.set_rate(rate_now as u32);
                mirror.push_block(&chunk);
            }
            let played = input.played_frames().saturating_sub(played_base);
            let ms = position_base_ms + (played as f64 * 1000.0 / rate_now as f64) as u64;
            shared.position_ms.store(ms, Ordering::Relaxed);
            shared.index.store(p.cur_index, Ordering::Relaxed);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
        if ended {
            pending.clear();
            let index = p.cur_index;
            let repeat = Repeat::from_u8(shared.repeat.load(Ordering::Relaxed));
            let advance = match repeat {
                Repeat::One => Some(index),
                Repeat::All => step_index(&shared, index, true),
                Repeat::Off => step_index_off(&shared, index),
            };
            match advance {
                Some(new_index) => {
                    program = None;
                    program_generation = shared.generation.load(Ordering::Relaxed);
                    shared.index.store(new_index, Ordering::Relaxed);
                    shared.position_ms.store(0, Ordering::Relaxed);
                    shared.seek_ms.store(u64::MAX, Ordering::Relaxed);
                }
                None => {
                    shared.playing.store(false, Ordering::Relaxed);
                    shared.finished.store(true, Ordering::Relaxed);
                    shared.position_ms.store(shared.duration_ms.load(Ordering::Relaxed), Ordering::Relaxed);
                    program = None;
                    shared.active.store(false, Ordering::Relaxed);
                }
            }
        }
    }
    shared.active.store(false, Ordering::Relaxed);
}

fn open_program(shared: &Arc<Shared>, index: usize) -> Option<Program> {
    let path = track_path(shared, index);
    if path.as_os_str().is_empty() {
        return None;
    }
    match StreamDecoder::open(&path) {
        Ok(decoder) => Some(Program::new(decoder, index)),
        Err(err) => {
            if let Ok(mut slot) = shared.error.lock() {
                *slot = Some(format!("{}: {err}", path.display()));
            }
            None
        }
    }
}

fn duration_for(shared: &Arc<Shared>, index: usize, program: &Program) -> u64 {
    let tracks = lock(&shared.tracks);
    if let Some(track) = tracks.get(index) {
        if let Some(ms) = track.duration_ms {
            if ms > 0 {
                return ms;
            }
        }
    }
    drop(tracks);
    program
        .cur
        .total_frames
        .map(|frames| (frames as f64 * 1000.0 / program.cur.rate() as f64) as u64)
        .unwrap_or(0)
}

fn track_path(shared: &Arc<Shared>, index: usize) -> PathBuf {
    lock(&shared.tracks)
        .get(index)
        .map(|t| t.path.clone())
        .unwrap_or_default()
}

fn fade_frames(shared: &Arc<Shared>, rate: u32) -> usize {
    if lock(&shared.tracks).len() < 2 {
        return 0;
    }
    (shared.crossfade_ms.load(Ordering::Relaxed) as u64 * rate as u64 / 1000) as usize
}

/// Next/previous with wrap-around, honouring shuffle.
fn step_index(shared: &Arc<Shared>, from: usize, forward: bool) -> Option<usize> {
    let len = lock(&shared.tracks).len();
    if len == 0 {
        return None;
    }
    if shared.shuffle.load(Ordering::Relaxed) && len > 1 {
        let mut next = from;
        // Cheap xorshift so a shuffle does not need a random crate.
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9)
            ^ shared.generation.load(Ordering::Relaxed);
        while next == from {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            next = (seed % len as u64) as usize;
        }
        return Some(next);
    }
    if forward {
        Some((from + 1) % len)
    } else if from == 0 {
        Some(len - 1)
    } else {
        Some(from - 1)
    }
}

/// End of the last track with repeat off: stop instead of wrapping.
fn step_index_off(shared: &Arc<Shared>, from: usize) -> Option<usize> {
    let len = lock(&shared.tracks).len();
    if len == 0 || from + 1 >= len {
        None
    } else {
        Some(from + 1)
    }
}

// ---------------------------------------------------------------------------
// Playlist files
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaylistFormat {
    Xspf,
    M3u,
    Pls,
    Unknown,
}

pub fn playlist_format(path: &Path) -> PlaylistFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "xspf" => PlaylistFormat::Xspf,
        "m3u" | "m3u8" => PlaylistFormat::M3u,
        "pls" => PlaylistFormat::Pls,
        _ => PlaylistFormat::Unknown,
    }
}

/// Read an XSPF, M3U/M3U8 or PLS playlist. Unknown extensions are sniffed by
/// content so a mislabelled file still loads.
pub fn load_playlist(path: &Path) -> Result<Vec<Track>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let format = match playlist_format(path) {
        PlaylistFormat::Unknown => {
            let probe = text.trim_start().trim_start_matches('\u{feff}');
            if probe.starts_with("<?xml") || probe.starts_with("<playlist") {
                PlaylistFormat::Xspf
            } else if probe.starts_with("[playlist]") {
                PlaylistFormat::Pls
            } else {
                PlaylistFormat::M3u
            }
        }
        other => other,
    };
    let tracks = match format {
        PlaylistFormat::Xspf => parse_xspf(&text, base),
        PlaylistFormat::M3u => parse_m3u(&text, base),
        PlaylistFormat::Pls => parse_pls(&text, base),
        PlaylistFormat::Unknown => Vec::new(),
    };
    if tracks.is_empty() {
        return Err("the playlist contains no audio files".to_string());
    }
    Ok(tracks)
}

fn parse_xspf(text: &str, base: &Path) -> Vec<Track> {
    let mut entries: Vec<(Option<String>, Track)> = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<track") {
        let after = &rest[start..];
        let Some(end) = after.find("</track>") else {
            break;
        };
        let block = &after[..end];
        let location = tag_text(block, "location");
        let title = tag_text(block, "title");
        let duration = tag_text(block, "duration").and_then(|d| d.trim().parse::<u64>().ok());
        let id = tag_text(block, "vlc:id").map(|s| s.trim().to_string());
        if let Some(location) = location {
            if let Some(path) = location_to_path(&location, base) {
                let mut track = Track::new(path).with_duration(duration);
                if let Some(title) = title {
                    let title = title.trim();
                    if !title.is_empty() {
                        track.title = title.to_string();
                    }
                }
                entries.push((id, track));
            }
        }
        rest = &after[end + "</track>".len()..];
    }
    if entries.is_empty() {
        return Vec::new();
    }

    // VLC writes the playback order as <vlc:item tid="N"/>; the <trackList>
    // order is insertion order and can differ.
    let mut order: Vec<usize> = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = text[cursor..].find("<vlc:item") {
        let chunk = &text[cursor + pos..];
        if let Some(tid) = attribute_value(chunk, "tid") {
            if let Some(index) = entries
                .iter()
                .position(|(id, _)| id.as_deref() == Some(tid.as_str()))
            {
                if !order.contains(&index) {
                    order.push(index);
                }
            }
        }
        cursor += pos + "<vlc:item".len();
    }
    for index in 0..entries.len() {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
        .into_iter()
        .filter_map(|index| entries.get(index).map(|(_, track)| track.clone()))
        .collect()
}

fn parse_m3u(text: &str, base: &Path) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut pending: Option<(Option<u64>, Option<String>)> = None;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            let (secs, title) = match info.split_once(',') {
                Some((secs, title)) => (secs, Some(title.to_string())),
                None => (info, None),
            };
            let duration = secs.trim().parse::<f64>().ok().map(|s| (s * 1000.0) as u64);
            pending = Some((duration, title));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(path) = entry_to_path(line, base) {
            let mut track = Track::new(path);
            if let Some((duration, title)) = pending.take() {
                track = track.with_duration(duration);
                if let Some(title) = title {
                    let title = title.trim();
                    if !title.is_empty() {
                        track.title = title.to_string();
                    }
                }
            }
            tracks.push(track);
        }
    }
    tracks
}

fn parse_pls(text: &str, base: &Path) -> Vec<Track> {
    use std::collections::BTreeMap;
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut titles: BTreeMap<u32, String> = BTreeMap::new();
    let mut lengths: BTreeMap<u32, u64> = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let (name, number) = match key.find(|c: char| c.is_ascii_digit()) {
            Some(pos) => (&key[..pos], key[pos..].parse::<u32>().ok()),
            None => (key, None),
        };
        let Some(number) = number else { continue };
        match name.to_ascii_lowercase().as_str() {
            "file" => {
                files.insert(number, value.to_string());
            }
            "title" => {
                titles.insert(number, value.to_string());
            }
            "length" => {
                if let Ok(secs) = value.parse::<i64>() {
                    if secs > 0 {
                        lengths.insert(number, secs as u64 * 1000);
                    }
                }
            }
            _ => {}
        }
    }
    files
        .into_iter()
        .filter_map(|(number, location)| {
            let path = entry_to_path(&location, base)?;
            let mut track = Track::new(path).with_duration(lengths.get(&number).copied());
            if let Some(title) = titles.get(&number) {
                let title = title.trim();
                if !title.is_empty() {
                    track.title = title.to_string();
                }
            }
            Some(track)
        })
        .collect()
}

fn tag_text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = block.find(&open)?;
    let after = &block[start..];
    let content_start = after.find('>')? + 1;
    let end = after[content_start..].find(&close)?;
    Some(unescape_xml(&after[content_start..content_start + end]))
}

fn attribute_value(chunk: &str, name: &str) -> Option<String> {
    let tag_end = chunk.find('>')?;
    let tag = &chunk[..tag_end];
    let pattern = format!("{name}=");
    let pos = tag.find(&pattern)? + pattern.len();
    let rest = tag[pos..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn entry_to_path(entry: &str, base: &Path) -> Option<PathBuf> {
    if entry.starts_with("http://") || entry.starts_with("https://") {
        // Network streams need an HTTP source in the decoder; flag them rather
        // than silently dropping the entry.
        return None;
    }
    if let Some(path) = location_to_path(entry, base) {
        return Some(path);
    }
    let decoded = percent_decode(entry);
    let candidate = PathBuf::from(decoded.trim());
    if candidate.is_absolute() {
        Some(candidate)
    } else {
        Some(base.join(candidate))
    }
}

/// `file:///C:/x/a.mp3` -> `C:\x\a.mp3`, decoding UTF-8 percent escapes (the
/// playlists contain emoji filenames, so this cannot be handled as bytes).
fn location_to_path(location: &str, base: &Path) -> Option<PathBuf> {
    let decoded = percent_decode(location.trim());
    let rest = decoded
        .strip_prefix("file:///")
        .or_else(|| decoded.strip_prefix("file://"))?;
    let rest = rest.strip_prefix("localhost/").unwrap_or(rest);
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let path = PathBuf::from(rest.replace('/', "\\"));
    if path.is_absolute() {
        Some(path)
    } else {
        Some(base.join(path))
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Write the playlist back out as XSPF so it can be opened elsewhere.
pub fn save_playlist(path: &Path, tracks: &[Track]) -> Result<(), String> {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <playlist xmlns=\"http://xspf.org/ns/0/\" xmlns:vlc=\"http://www.videolan.org/vlc/playlist/ns/0/\" version=\"1\">\n\
         \t<title>Playlist</title>\n\t<trackList>\n",
    );
    for (index, track) in tracks.iter().enumerate() {
        out.push_str("\t\t<track>\n");
        out.push_str(&format!(
            "\t\t\t<location>{}</location>\n",
            escape_xml(&path_to_location(&track.path))
        ));
        if let Some(ms) = track.duration_ms {
            out.push_str(&format!("\t\t\t<duration>{ms}</duration>\n"));
        }
        out.push_str(&format!(
            "\t\t\t<title>{}</title>\n\t\t\t<extension application=\"http://www.videolan.org/vlc/playlist/0\">\n\t\t\t\t<vlc:id>{index}</vlc:id>\n\t\t\t</extension>\n\t\t</track>\n",
            escape_xml(&track.title)
        ));
    }
    out.push_str("\t</trackList>\n\t<extension application=\"http://www.videolan.org/vlc/playlist/0\">\n");
    for index in 0..tracks.len() {
        out.push_str(&format!("\t\t<vlc:item tid=\"{index}\"/>\n"));
    }
    out.push_str("\t</extension>\n</playlist>\n");
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

fn path_to_location(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decodes_utf8_and_spaces() {
        assert_eq!(
            percent_decode("HAPPY%20TECHNO%20MIX%20%F0%9F%92%A3.mp3"),
            "HAPPY TECHNO MIX \u{1f4a3}.mp3"
        );
    }

    #[test]
    fn xspf_orders_by_vlc_items() {
        let xml = r#"<?xml version="1.0"?>
        <playlist xmlns="http://xspf.org/ns/0/" xmlns:vlc="http://www.videolan.org/vlc/playlist/ns/0/" version="1">
          <trackList>
            <track><location>file:///C:/b.mp3</location><duration>2000</duration><extension><vlc:id>0</vlc:id></extension></track>
            <track><location>file:///C:/a.mp3</location><duration>1000</duration><extension><vlc:id>1</vlc:id></extension></track>
          </trackList>
          <extension><vlc:item tid="1"/><vlc:item tid="0"/></extension>
        </playlist>"#;
        let tracks = parse_xspf(xml, Path::new("C:/"));
        assert_eq!(tracks.len(), 2);
        assert!(tracks[0].path.ends_with("a.mp3"));
        assert!(tracks[1].path.ends_with("b.mp3"));
        assert_eq!(tracks[0].duration_ms, Some(1000));
    }

    #[test]
    fn m3u_reads_duration_and_title() {
        let text = "#EXTM3U\n#EXTINF:125,Some Title\nC:\\Music\\one.mp3\n#EXTINF:-1,Two\nC:\\Music\\two.mp3\n";
        let tracks = parse_m3u(text, Path::new("C:/"));
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Some Title");
        assert_eq!(tracks[0].duration_ms, Some(125_000));
        assert_eq!(tracks[1].title, "Two");
        assert_eq!(tracks[1].duration_ms, None);
    }

    #[test]
    fn pls_reads_files_in_order() {
        let text = "[playlist]\nFile2=C:\\b.mp3\nTitle2=B\nFile1=C:\\a.mp3\nTitle1=A\nLength1=7\nNumberOfEntries=2\n";
        let tracks = parse_pls(text, Path::new("C:/"));
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "A");
        assert_eq!(tracks[0].duration_ms, Some(7000));
        assert_eq!(tracks[1].title, "B");
        assert_eq!(tracks[1].duration_ms, None);
        assert!(tracks[1].path.ends_with("b.mp3"));
    }

    #[test]
    fn relative_m3u_paths_resolve_against_playlist_folder() {
        let tracks = parse_m3u("tracks/one.mp3\n", Path::new("C:/Music"));
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].path.ends_with("Music\\tracks\\one.mp3") || tracks[0].path.ends_with("Music/tracks/one.mp3"));
    }

    /// The streaming decoder must hand out a whole file without ever holding it
    /// in RAM (the old tape deck decoded everything up front and capped at 30
    /// minutes). A generated 2 s WAV proves open/seek/EOF.
    #[test]
    fn streams_a_wav_without_loading_it_into_ram() {
        let path = std::env::temp_dir().join("mixer_player_stream_test.wav");
        let rate = 48_000u32;
        let seconds = 2u32;
        let frames = rate * seconds;
        let data_len = frames * 4;
        let mut data = Vec::with_capacity(44 + data_len as usize);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 4).to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let v = (n as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            let s = (v * i16::MAX as f32) as i16;
            data.extend_from_slice(&s.to_le_bytes());
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, &data).expect("write test wav");

        let mut decoder = StreamDecoder::open(&path).expect("open");
        assert_eq!(decoder.rate(), 48_000);
        let mut peak = 0.0f32;
        let mut count = 0u64;
        while let Some(frame) = decoder.next_frame() {
            peak = peak.max(frame[0].abs()).max(frame[1].abs());
            count += 1;
        }
        assert!(count + 64 >= frames as u64, "decoded {count} of {frames}");
        assert!(peak > 0.3, "peak {peak}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seeks_within_a_stream() {
        let path = std::env::temp_dir().join("mixer_player_seek_test.wav");
        let rate = 48_000u32;
        let frames = rate; // 1 s
        let data_len = frames * 4;
        let mut data = Vec::with_capacity(44 + data_len as usize);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 4).to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let v = if n < rate / 2 { 0.1 } else { 0.6 };
            let s = (v * i16::MAX as f32) as i16;
            data.extend_from_slice(&s.to_le_bytes());
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, &data).expect("write test wav");

        let mut decoder = StreamDecoder::open(&path).expect("open");
        decoder.seek_ms(700).expect("seek");
        let frame = decoder.next_frame().expect("frame after seek");
        assert!(frame[0].abs() > 0.4, "expected the loud half, got {}", frame[0]);
        let _ = std::fs::remove_file(&path);
    }

    /// Uses a real playlist when the machine has one, so the VLC ordering and
    /// the emoji filenames are exercised without shipping test data.
    #[test]
    fn loads_a_real_vlc_playlist_when_present() {
        let path = Path::new("C:/Users/jayge/Music/NCS/Happy Techno.xspf");
        if !path.exists() {
            return;
        }
        let tracks = load_playlist(path).expect("load playlist");
        assert!(tracks.len() >= 2);
        assert!(tracks.iter().all(|t| t.path.is_absolute()));
        assert!(tracks[0].path.is_file());
        assert!(tracks[0].duration_ms.unwrap_or(0) > 60_000);
    }

    /// End-to-end worker check without an audio device: pressing Play must make
    /// the decoder thread push frames into the strip's ring.
    #[test]
    fn worker_fills_the_ring_for_a_playing_track() {
        let path = std::env::temp_dir().join("mixer_player_worker_test.wav");
        let rate = 48_000u32;
        let frames = rate; // 1 s
        let data_len = frames * 4;
        let mut data = Vec::with_capacity(44 + data_len as usize);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&rate.to_le_bytes());
        data.extend_from_slice(&(rate * 4).to_le_bytes());
        data.extend_from_slice(&4u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for n in 0..frames {
            let v = (n as f32 / rate as f32 * 330.0 * std::f32::consts::TAU).sin() * 0.4;
            let s = (v * i16::MAX as f32) as i16;
            data.extend_from_slice(&s.to_le_bytes());
            data.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, &data).expect("write test wav");

        let input = Arc::new(crate::audio::InputShared::new());
        let player = Player::new(Arc::clone(&input));
        player.set_tracks(vec![Track::new(path.clone()).with_duration(Some(1000))], 0);
        player.play();
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            input.ring_fill() > 0,
            "the worker pushed no frames into the strip ring"
        );
        player.stop();
        drop(player);
        let _ = std::fs::remove_file(&path);
    }
}
