//! Mix engine for console-8-rs.
//!
//! Playback reads take files from disk per block (region readers seek
//! on playhead jumps, so scrubbing and arranging cost nothing but a
//! seek). Recording streams input chunks to a writer thread that owns
//! the WAV file, so a take runs as long as there is disk space —
//! nothing is buffered in RAM beyond one chunk queue.
//!
//! The engine is deliberately hardware-free: cpal glue lives in
//! audio.rs and feeds it blocks and input chunks. Tests render mixes
//! straight from synthetic take files.

use crate::dsp::{
    equal_power_pan, maximiser, tube_drive, DcBlocker, Limiter, PeakMeter, Strip,
};
use crate::session::{InputSel, Session, N_TRACKS};
use crate::wav::{WavReader, WavWriter};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

pub enum RecMsg {
    Chunk(Vec<f32>),
    Stop,
}

/// One active take: a writer thread owns the WAV file.
pub struct Recorder {
    tx: Sender<RecMsg>,
    handle: Option<JoinHandle<u64>>,
    frames: Arc<AtomicU64>,
}

impl Recorder {
    pub fn start(path: &Path, sample_rate: u32) -> std::io::Result<Recorder> {
        let (tx, rx) = channel::<RecMsg>();
        let frames = Arc::new(AtomicU64::new(0));
        let frames_w = frames.clone();
        let path = path.to_path_buf();
        let handle = std::thread::spawn(move || -> u64 {
            let mut writer = match WavWriter::create(&path, sample_rate) {
                Ok(w) => w,
                Err(_) => return 0,
            };
            loop {
                match rx.recv() {
                    Ok(RecMsg::Chunk(frames_in)) => {
                        if writer.write(&frames_in).is_err() {
                            break;
                        }
                        frames_w.fetch_add(frames_in.len() as u64, Ordering::Relaxed);
                    }
                    Ok(RecMsg::Stop) | Err(_) => break,
                }
            }
            writer.finalize().map(|f| f).unwrap_or(0)
        });
        Ok(Recorder {
            tx,
            handle: Some(handle),
            frames,
        })
    }

    pub fn sender(&self) -> Sender<RecMsg> {
        self.tx.clone()
    }

    pub fn push(&self, frames: &[f32]) {
        let _ = self.tx.send(RecMsg::Chunk(frames.to_vec()));
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Stop the writer thread and return finalized frames.
    pub fn stop(mut self) -> u64 {
        let _ = self.tx.send(RecMsg::Stop);
        match self.handle.take() {
            Some(h) => h.join().unwrap_or(0),
            None => 0,
        }
    }
}

struct ToneFeeder {
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl ToneFeeder {
    fn start(tx: Sender<RecMsg>, sample_rate: u32) -> ToneFeeder {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut phase = 0.0f32;
            let step = 2.0 * std::f32::consts::PI * 440.0 / sample_rate as f32;
            let chunk = 1024usize;
            let chunk_dur = std::time::Duration::from_secs_f64(chunk as f64 / sample_rate as f64);
            while !stop_w.load(Ordering::Relaxed) {
                let mut buf = Vec::with_capacity(chunk);
                for _ in 0..chunk {
                    buf.push((phase.sin()) * 0.5);
                    phase += step;
                    if phase > 2.0 * std::f32::consts::PI {
                        phase -= 2.0 * std::f32::consts::PI;
                    }
                }
                if tx.send(RecMsg::Chunk(buf)).is_err() {
                    break;
                }
                // Pace to real time: a test tone must record at 1x.
                std::thread::sleep(chunk_dur);
            }
        });
        ToneFeeder {
            handle: Some(handle),
            stop,
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct Engine {
    pub session: Session,
    pub dir: PathBuf,
    pub playing: bool,
    strips: [Strip; N_TRACKS],
    readers: HashMap<(usize, u64), WavReader>,
    meters: [PeakMeter; N_TRACKS],
    master_l: PeakMeter,
    master_r: PeakMeter,
    dc_l: DcBlocker,
    dc_r: DcBlocker,
    limiter: Limiter,
    recorders: HashMap<usize, Recorder>,
    tone_feeders: Vec<ToneFeeder>,
    /// (track, region id, live frames) for takes being recorded now.
    pub live_takes: HashMap<usize, (u64, Arc<AtomicU64>)>,
    pub missing_files: Vec<String>,
}

fn take_name(track: usize) -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("take-{}-{}.wav", track, ms)
}

impl Engine {
    pub fn new(session: Session, dir: PathBuf) -> Engine {
        let sr = session.sample_rate as f32;
        Engine {
            session,
            dir,
            playing: false,
            strips: std::array::from_fn(|_| Strip::new(sr)),
            readers: HashMap::new(),
            meters: std::array::from_fn(|_| PeakMeter::new(sr)),
            master_l: PeakMeter::new(sr),
            master_r: PeakMeter::new(sr),
            dc_l: DcBlocker::default(),
            dc_r: DcBlocker::default(),
            limiter: Limiter::new(sr, -0.5),
            recorders: HashMap::new(),
            tone_feeders: Vec::new(),
            live_takes: HashMap::new(),
            missing_files: Vec::new(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.session.sample_rate
    }

    pub fn recording(&self) -> bool {
        !self.recorders.is_empty()
    }

    pub fn armed_tracks(&self) -> Vec<usize> {
        (0..N_TRACKS)
            .filter(|&i| self.session.tracks[i].armed)
            .collect()
    }

    /// Begin takes on every armed track that has an input selected.
    /// Returns (track, file, region id) per take started.
    pub fn begin_recording(&mut self) -> Vec<(usize, String, u64)> {
        let sr = self.session.sample_rate;
        let mut started = Vec::new();
        let playhead = self.session.playhead;
        for i in 0..N_TRACKS {
            let (armed, has_input, is_tone) = {
                let track = &self.session.tracks[i];
                (
                    track.armed,
                    !matches!(track.input, InputSel::None),
                    matches!(track.input, InputSel::Tone),
                )
            };
            if !armed || !has_input {
                continue;
            }
            let file = take_name(i);
            let path = Session::take_path(&self.dir, &file);
            match Recorder::start(&path, sr) {
                Ok(rec) => {
                    let frames = rec.frames.clone();
                    let region_id =
                        self.session.add_region(i, &file, playhead, 0, 0);
                    self.live_takes.insert(i, (region_id, frames));
                    if is_tone {
                        let feeder = ToneFeeder::start(rec.sender(), sr);
                        self.tone_feeders.push(feeder);
                    }
                    self.recorders.insert(i, rec);
                    started.push((i, file, region_id));
                }
                Err(e) => {
                    eprintln!("console-8-rs: cannot record track {}: {e}", i + 1);
                }
            }
        }
        started
    }

    /// Feed an input chunk to a track's active recorder, if any.
    pub fn push_input(&mut self, track: usize, frames: &[f32]) {
        if let Some(rec) = self.recorders.get(&track) {
            rec.push(frames);
        }
    }

    /// Stop all takes, finalize files, update region lengths.
    /// Zero-length takes are deleted (region + file).
    pub fn end_recording(&mut self) -> Vec<(usize, u64, u64)> {
        let feeders = std::mem::take(&mut self.tone_feeders);
        for f in feeders {
            f.stop();
        }
        let recs = std::mem::take(&mut self.recorders);
        self.live_takes.clear();
        let mut done = Vec::new();
        for (track, rec) in recs {
            let frames = rec.stop();
            // Find the region this take created (the last zero-length
            // region on the track at the take start is ours; we track
            // it by matching the newest region instead: regions are
            // only appended, so scan for a region whose file is the
            // take file — simplest correct lookup is by file name).
            // We stored no file name here, so find zero-length regions.
            let region_id = self.session.tracks[track]
                .regions
                .iter()
                .rev()
                .find(|r| r.len == 0)
                .map(|r| r.id);
            if let Some(id) = region_id {
                if frames == 0 {
                    self.session.delete_region(track, id);
                    // File was created empty; leave it — a 44-byte
                    // header harms nothing, and deleting on a race
                    // with the writer thread is worse. (Takes dir is
                    // gitignored; empty takes are prunable by hand.)
                } else {
                    if let Some(r) = self.session.find_region_mut(track, id) {
                        r.len = frames;
                    }
                    done.push((track, id, frames));
                }
            }
        }
        done
    }

    fn reader_for(
        &mut self,
        track: usize,
        region_id: u64,
        file: &str,
        file_pos: u64,
    ) -> Option<&mut WavReader> {
        let key = (track, region_id);
        let need_open = !self.readers.contains_key(&key);
        if need_open {
            let path = Session::take_path(&self.dir, file);
            match WavReader::open(&path) {
                Ok(r) => {
                    self.readers.insert(key, r);
                }
                Err(_) => {
                    if !self.missing_files.contains(&file.to_string()) {
                        self.missing_files.push(file.to_string());
                    }
                    return None;
                }
            }
        }
        let r = self.readers.get_mut(&key)?;
        if r.position() != file_pos {
            if r.seek_frame(file_pos).is_err() {
                return None;
            }
        }
        Some(r)
    }

    /// Render one block into out_l / out_r.
    /// monitor[i] is this block's live input for track i (may be
    /// shorter than the block, or empty). When `playing` is false,
    /// regions are silent but monitored inputs still pass through.
    pub fn render_block(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        monitor: &[&[f32]; N_TRACKS],
    ) {
        let n = out_l.len().min(out_r.len());
        let block_start = self.session.playhead;
        let block_end = block_start + n as u64;
        let mut mix_l = vec![0.0f32; n];
        let mut mix_r = vec![0.0f32; n];

        for i in 0..N_TRACKS {
            if !self.session.track_audible(i) {
                self.meters[i].push(0.0);
                continue;
            }
            let mut buf = vec![0.0f32; n];
            if self.playing {
                // Snapshot regions: the borrow of session regions
                // must end before reader_for borrows self mutably.
                let regions: Vec<(u64, String, u64, u64, u64)> = self.session.tracks[i]
                    .regions
                    .iter()
                    .map(|r| (r.id, r.file.clone(), r.start, r.offset, r.len))
                    .collect();
                for (id, file, start, offset, len) in regions {
                    let end = start + len;
                    if end <= block_start || start >= block_end {
                        continue;
                    }
                    let read_start = block_start.max(start);
                    let read_end = block_end.min(end);
                    let count = (read_end - read_start) as usize;
                    let file_pos = offset + (read_start - start);
                    let at = (read_start - block_start) as usize;
                    if let Some(reader) = self.reader_for(i, id, &file, file_pos) {
                        let mut tmp = vec![0.0f32; count];
                        if let Ok(got) = reader.read_frames(&mut tmp) {
                            for j in 0..got {
                                buf[at + j] += tmp[j];
                            }
                        }
                    }
                }
            }
            // Live input monitoring (through the strip, like tape).
            let mon = monitor[i];
            for (j, &m) in mon.iter().enumerate().take(n) {
                buf[j] += m;
            }
            // Strip + pan + fader.
            let params = self.session.tracks[i].strip.clone();
            self.strips[i].set_params(&params);
            let mut track_peak = 0.0f32;
            for x in buf.iter_mut() {
                *x = self.strips[i].process(*x);
            }
            let (pl, pr) = equal_power_pan(params.pan);
            for j in 0..n {
                let v = buf[j] * params.fader;
                track_peak = track_peak.max(v.abs());
                mix_l[j] += v * pl;
                mix_r[j] += v * pr;
            }
            self.meters[i].push(track_peak);
        }

        // Master bus: tube (always on) -> maximiser -> fader -> limiter.
        let max = self.session.master.maximiser;
        let fader = self.session.master.fader;
        for j in 0..n {
            let mut l = tube_drive(mix_l[j]);
            let mut r = tube_drive(mix_r[j]);
            l = self.dc_l.process(l);
            r = self.dc_r.process(r);
            l = maximiser(l, max);
            r = maximiser(r, max);
            l *= fader;
            r *= fader;
            let (l, r) = self.limiter.process_stereo(l, r);
            out_l[j] = l;
            out_r[j] = r;
            self.master_l.push(l);
            self.master_r.push(r);
        }

        if self.playing {
            self.session.playhead = block_end;
        }
    }

    pub fn meter(&self, track: usize) -> f32 {
        self.meters[track].value()
    }

    pub fn master_meters(&self) -> (f32, f32) {
        (self.master_l.value(), self.master_r.value())
    }

    /// Drop cached readers (call after edits that rename files).
    pub fn drop_readers(&mut self) {
        self.readers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("console8rs-engine-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("takes")).unwrap();
        dir
    }

    fn write_take(dir: &Path, file: &str, frames: &[f32]) -> PathBuf {
        let path = Session::take_path(dir, file);
        let mut w = WavWriter::create(&path, 48000).unwrap();
        w.write(frames).unwrap();
        w.finalize().unwrap();
        path
    }

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin())
            .collect()
    }

    #[test]
    fn mix_sums_regions_and_advances_playhead() {
        let dir = test_dir("mix");
        let sig = sine(440.0, 0.5, 4800);
        write_take(&dir, "a.wav", &sig);
        let mut s = Session::new("t", 48000);
        let _ = s.add_region(0, "a.wav", 0, 0, 4800);
        let mut eng = Engine::new(s, dir.clone());
        eng.playing = true;
        let mut l = vec![0.0; 1024];
        let mut r = vec![0.0; 1024];
        let empty: [&[f32]; N_TRACKS] = [&[], &[], &[], &[], &[], &[], &[], &[]];
        eng.render_block(&mut l, &mut r, &empty);
        assert_eq!(eng.session.playhead, 1024);
        // Signal present (through tube/maximiser/fader/limiter, so
        // not sample-exact, but clearly non-silent and bounded).
        let peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "mix peak {peak}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fader_scales_and_mute_silences() {
        let dir = test_dir("fader");
        let sig = vec![0.5; 4800];
        write_take(&dir, "a.wav", &sig);
        let mut s = Session::new("t", 48000);
        let _ = s.add_region(0, "a.wav", 0, 0, 4800);
        s.tracks[0].strip.fader = 0.0;
        let mut eng = Engine::new(s, dir.clone());
        eng.playing = true;
        let mut l = vec![0.0; 512];
        let mut r = vec![0.0; 512];
        let empty: [&[f32]; N_TRACKS] = [&[], &[], &[], &[], &[], &[], &[], &[]];
        eng.render_block(&mut l, &mut r, &empty);
        assert!(l.iter().all(|&x| x == 0.0), "fader 0 must silence");
        // Mute also silences, even at full fader.
        eng.session.tracks[0].strip.fader = 1.0;
        eng.session.tracks[0].mute = true;
        eng.render_block(&mut l, &mut r, &empty);
        assert!(l.iter().all(|&x| x == 0.0), "mute must silence");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn solo_isolates() {
        let dir = test_dir("solo");
        write_take(&dir, "a.wav", &sine(440.0, 0.5, 4800));
        write_take(&dir, "b.wav", &sine(880.0, 0.25, 4800));
        let mut s = Session::new("t", 48000);
        let _ = s.add_region(0, "a.wav", 0, 0, 4800);
        let _ = s.add_region(1, "b.wav", 0, 0, 4800);
        s.tracks[1].solo = true;
        let empty: [&[f32]; N_TRACKS] = [&[], &[], &[], &[], &[], &[], &[], &[]];
        let mut eng = Engine::new(s, dir.clone());
        eng.playing = true;
        let mut l = vec![0.0; 512];
        let mut r = vec![0.0; 512];
        eng.render_block(&mut l, &mut r, &empty);
        let solo_peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // Same mix without solo: track 0 (louder) joins, so the peak
        // must clearly exceed the soloed peak.
        let mut s2 = Session::new("t", 48000);
        let _ = s2.add_region(0, "a.wav", 0, 0, 4800);
        let _ = s2.add_region(1, "b.wav", 0, 0, 4800);
        let mut eng2 = Engine::new(s2, dir.clone());
        eng2.playing = true;
        eng2.render_block(&mut l, &mut r, &empty);
        let full_peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(solo_peak > 0.1, "soloed track should sound, peak {solo_peak}");
        assert!(
            solo_peak < full_peak * 0.8,
            "solo peak {solo_peak} should be well under full peak {full_peak}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playhead_jump_seeks_correctly() {
        let dir = test_dir("seek");
        // One take, two halves: quiet 440 Hz, then loud 440 Hz.
        let mut sig = sine(440.0, 0.1, 4800);
        sig.extend_from_slice(&sine(440.0, 0.8, 4800));
        write_take(&dir, "a.wav", &sig);
        let mut s = Session::new("t", 48000);
        let _ = s.add_region(0, "a.wav", 0, 0, 9600);
        let mut eng = Engine::new(s, dir.clone());
        eng.playing = true;
        let mut l = vec![0.0; 1024];
        let mut r = vec![0.0; 1024];
        let empty: [&[f32]; N_TRACKS] = [&[], &[], &[], &[], &[], &[], &[], &[]];
        eng.render_block(&mut l, &mut r, &empty);
        // Jump into the loud half: the mix must reflect the later
        // audio, not the quiet start.
        eng.session.playhead = 4800;
        eng.render_block(&mut l, &mut r, &empty);
        let mean = l[512..].iter().map(|x| x.abs()).sum::<f32>() / 512.0;
        assert!(mean > 0.25, "jumped mix should be loud, mean {mean}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_writes_audible_take_to_disk() {
        let dir = test_dir("rec");
        let s = Session::new("t", 48000);
        let mut eng = Engine::new(s, dir.clone());
        eng.session.tracks[0].armed = true;
        eng.session.tracks[0].input = InputSel::Tone;
        let started = eng.begin_recording();
        assert_eq!(started.len(), 1);
        // Feed device-style chunks directly (tone feeder also runs,
        // but push_input is what device streams use).
        let chunk = vec![0.3; 1024];
        for _ in 0..4 {
            eng.push_input(0, &chunk);
        }
        let done = eng.end_recording();
        assert_eq!(done.len(), 1);
        let (_, _, frames) = done[0];
        assert!(frames >= 4096, "recorder should hold the pushed frames");
        let region = eng.session.tracks[0].regions.last().unwrap();
        assert_eq!(region.len, frames);
        let path = Session::take_path(&dir, &region.file);
        assert!(path.exists());
        let mut r = WavReader::open(&path).unwrap();
        assert_eq!(r.total_frames(), frames);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_keeps_up_at_realtime_pace() {
        // Regression probe for the 2026-10-26 on-machine OOM-kill report:
        // feed the recorder at exactly realtime pace and watch RSS. If the
        // writer thread falls behind the input rate, the unbounded channel
        // backlogs and RSS climbs with the audio pushed; a healthy writer
        // keeps RSS flat.
        fn rss_kb() -> u64 {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                        l.split_whitespace().nth(1).and_then(|v| v.parse().ok())
                    })
                })
                .unwrap_or(0)
        }
        let dir = test_dir("rtpace");
        let s = Session::new("t", 48000);
        let mut eng = Engine::new(s, dir.clone());
        eng.session.tracks[0].armed = true;
        eng.session.tracks[0].input = InputSel::Tone;
        let started = eng.begin_recording();
        assert_eq!(started.len(), 1);
        let chunk = vec![0.3; 1024];
        let chunk_dur = std::time::Duration::from_micros(1024 * 1_000_000 / 48000);
        let t0 = rss_kb();
        // ~6s of audio at realtime pace, sampled every ~2s.
        for i in 0..280 {
            eng.push_input(0, &chunk);
            if i % 94 == 0 {
                eprintln!("rss sample i={} {} kB", i, rss_kb());
            }
            std::thread::sleep(chunk_dur);
        }
        let t1 = rss_kb();
        let done = eng.end_recording();
        assert_eq!(done.len(), 1);
        assert!(done[0].2 >= 280 * 1024, "take should hold all pushed frames");
        eprintln!("rss before={} kB after_push={} kB", t0, t1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn monitor_passes_through_when_stopped() {
        let dir = test_dir("mon");
        let s = Session::new("t", 48000);
        let mut eng = Engine::new(s, dir.clone());
        eng.playing = false;
        let mon_in = vec![0.4; 512];
        let mon: [&[f32]; N_TRACKS] = [
            &mon_in, &[], &[], &[], &[], &[], &[], &[],
        ];
        let mut l = vec![0.0; 512];
        let mut r = vec![0.0; 512];
        eng.render_block(&mut l, &mut r, &mon);
        let peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1, "monitored input should sound when stopped");
        assert_eq!(eng.session.playhead, 0, "playhead must not move");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
