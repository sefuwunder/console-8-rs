//! Session model for console-8-rs: 8 tracks, regions that reference
//! take files on disk, strip + master parameters, and the tape edits:
//! split (splice), move, trim, delete. Saved as plain text so a session
//! stays readable and repairable by hand.
//!
//! Layout on disk:
//!   <session dir>/session.txt   (this format)
//!   <session dir>/takes/*.wav   (takes, referenced by file name only)

use crate::dsp::StripParams;
use std::path::{Path, PathBuf};

pub const N_TRACKS: usize = 8;
pub const SESSION_FILE: &str = "session.txt";
pub const TAKES_DIR: &str = "takes";

#[derive(Clone, Debug, PartialEq)]
pub enum InputSel {
    None,
    Tone,
    Device(String),
    /// PipeWire node name (a mic or a sink monitor for internal app
    /// audio). Resolved to a live node id at capture start; names are
    /// stable across daemon restarts, ids are not.
    PipeWire(String),
}

impl InputSel {
    /// Short label for the track header (device names get abbreviated).
    pub fn label(&self) -> String {
        match self {
            InputSel::None => "NO INPUT".to_string(),
            InputSel::Tone => "TONE".to_string(),
            InputSel::Device(name) => {
                let short: String = name.chars().take(14).collect();
                format!("{short}")
            }
            InputSel::PipeWire(name) => {
                let short: String = name.chars().take(14).collect();
                format!("{short}")
            }
        }
    }

    fn encode(&self) -> String {
        match self {
            InputSel::None => "none".to_string(),
            InputSel::Tone => "tone".to_string(),
            InputSel::Device(name) => format!("dev:{}", escape(name)),
            InputSel::PipeWire(name) => format!("pw:{}", escape(name)),
        }
    }

    fn decode(s: &str) -> Option<InputSel> {
        match s {
            "none" => Some(InputSel::None),
            "tone" => Some(InputSel::Tone),
            _ => s
                .strip_prefix("dev:")
                .map(|rest| InputSel::Device(unescape(rest)))
                .or_else(|| {
                    s.strip_prefix("pw:")
                        .map(|rest| InputSel::PipeWire(unescape(rest)))
                }),
        }
    }
}

/// A region is a window onto a take file: it plays file samples
/// [offset, offset+len) at timeline position [start, start+len).
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub id: u64,
    pub file: String,
    pub start: u64,
    pub offset: u64,
    pub len: u64,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    pub fn contains(&self, t: u64) -> bool {
        t >= self.start && t < self.end()
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub name: String,
    pub input: InputSel,
    pub armed: bool,
    pub mute: bool,
    pub solo: bool,
    pub strip: StripParams,
    pub regions: Vec<Region>,
}

impl Track {
    fn new(i: usize) -> Self {
        Track {
            name: format!("TRACK {}", i + 1),
            input: InputSel::None,
            armed: false,
            mute: false,
            solo: false,
            strip: StripParams::default(),
            regions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MasterParams {
    pub maximiser: f32,
    pub fader: f32,
}

impl Default for MasterParams {
    fn default() -> Self {
        MasterParams {
            maximiser: 0.5,
            fader: 1.0,
        }
    }
}

pub struct Session {
    pub name: String,
    pub sample_rate: u32,
    pub playhead: u64,
    pub tracks: [Track; N_TRACKS],
    pub master: MasterParams,
    pub next_region_id: u64,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('|') => out.push('|'),
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a line on unescaped `|` separators.
fn split_escaped(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '|' || next == '\\' || next == 'n' {
                    cur.push('\\');
                    cur.push(chars.next().unwrap());
                    continue;
                }
            }
            cur.push(c);
        } else if c == '|' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

impl Session {
    pub fn new(name: &str, sample_rate: u32) -> Self {
        Session {
            name: name.to_string(),
            sample_rate,
            playhead: 0,
            tracks: std::array::from_fn(|i| Track::new(i)),
            master: MasterParams::default(),
            next_region_id: 1,
        }
    }

    pub fn any_soloed(&self) -> bool {
        self.tracks.iter().any(|t| t.solo)
    }

    /// Is this track audible (mute/solo rules)?
    pub fn track_audible(&self, i: usize) -> bool {
        let t = &self.tracks[i];
        if t.mute {
            return false;
        }
        if self.any_soloed() && !t.solo {
            return false;
        }
        true
    }

    pub fn add_region(
        &mut self,
        track: usize,
        file: &str,
        start: u64,
        offset: u64,
        len: u64,
    ) -> u64 {
        let id = self.next_region_id;
        self.next_region_id += 1;
        self.tracks[track].regions.push(Region {
            id,
            file: file.to_string(),
            start,
            offset,
            len,
        });
        id
    }

    pub fn find_region(&self, track: usize, id: u64) -> Option<&Region> {
        self.tracks[track].regions.iter().find(|r| r.id == id)
    }

    pub fn find_region_mut(&mut self, track: usize, id: u64) -> Option<&mut Region> {
        self.tracks[track].regions.iter_mut().find(|r| r.id == id)
    }

    pub fn region_at(&self, track: usize, t: u64) -> Option<&Region> {
        self.tracks[track].regions.iter().find(|r| r.contains(t))
    }

    /// Splice: split a region into two at timeline position `at`.
    /// Returns the new region's id, or None if `at` is not inside.
    pub fn split_region(&mut self, track: usize, id: u64, at: u64) -> Option<u64> {
        let idx = self.tracks[track].regions.iter().position(|r| r.id == id)?;
        let r = self.tracks[track].regions[idx].clone();
        if at <= r.start || at >= r.end() {
            return None;
        }
        let left_len = at - r.start;
        self.tracks[track].regions[idx].len = left_len;
        let new_id = self.next_region_id;
        self.next_region_id += 1;
        self.tracks[track].regions.push(Region {
            id: new_id,
            file: r.file,
            start: at,
            offset: r.offset + left_len,
            len: r.len - left_len,
        });
        Some(new_id)
    }

    pub fn move_region(&mut self, track: usize, id: u64, new_track: usize, new_start: u64) -> bool {
        let idx = match self.tracks[track].regions.iter().position(|r| r.id == id) {
            Some(i) => i,
            None => return false,
        };
        let mut r = self.tracks[track].regions.remove(idx);
        r.start = new_start;
        self.tracks[new_track].regions.push(r);
        true
    }

    pub fn delete_region(&mut self, track: usize, id: u64) -> bool {
        let before = self.tracks[track].regions.len();
        self.tracks[track].regions.retain(|r| r.id != id);
        self.tracks[track].regions.len() != before
    }

    /// Trim the head: move the region start later, keeping the tail.
    pub fn trim_region_start(&mut self, track: usize, id: u64, new_start: u64) -> bool {
        let r = match self.find_region_mut(track, id) {
            Some(r) => r,
            None => return false,
        };
        if new_start <= r.start || new_start >= r.end() {
            return false;
        }
        let delta = new_start - r.start;
        r.offset += delta;
        r.len -= delta;
        r.start = new_start;
        true
    }

    /// Trim the tail: shorten the region from the end.
    pub fn trim_region_end(&mut self, track: usize, id: u64, new_end: u64) -> bool {
        let r = match self.find_region_mut(track, id) {
            Some(r) => r,
            None => return false,
        };
        if new_end <= r.start || new_end >= r.end() {
            return false;
        }
        r.len = new_end - r.start;
        true
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "session|1|{}|{}|{}|{}|{}|{}\n",
            escape(&self.name),
            self.sample_rate,
            self.playhead,
            self.master.maximiser,
            self.master.fader,
            self.next_region_id
        ));
        for (i, t) in self.tracks.iter().enumerate() {
            let s = &t.strip;
            out.push_str(&format!(
                "track|{}|{}|{}|{}|{}|{}|{} {} {} {} {} {} {} {} {} {} {} {} {}\n",
                i,
                escape(&t.name),
                t.input.encode(),
                t.armed as u8,
                t.mute as u8,
                t.solo as u8,
                s.trim_db,
                s.polarity as u8,
                s.hpf_freq,
                s.lf_gain,
                s.lmf_gain,
                s.lmf_freq,
                s.hmf_gain,
                s.hmf_freq,
                s.hf_gain,
                s.comp_threshold_db,
                s.comp_ratio,
                s.pan,
                s.fader
            ));
        }
        for (i, t) in self.tracks.iter().enumerate() {
            for r in &t.regions {
                out.push_str(&format!(
                    "region|{}|{}|{}|{}|{}|{}\n",
                    i,
                    r.id,
                    escape(&r.file),
                    r.start,
                    r.offset,
                    r.len
                ));
            }
        }
        out
    }

    pub fn from_text(text: &str) -> Result<Session, String> {
        let mut session: Option<Session> = None;
        for (lineno, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let f = split_escaped(line);
            match f.first().map(|s| s.as_str()) {
                Some("session") => {
                    if f.len() != 8 {
                        return Err(format!("line {}: bad session line", lineno + 1));
                    }
                    let mut s = Session::new(
                        &unescape(&f[2]),
                        f[3].parse().map_err(|_| "bad sample rate")?,
                    );
                    s.playhead = f[4].parse().map_err(|_| "bad playhead")?;
                    s.master.maximiser = f[5].parse().map_err(|_| "bad maximiser")?;
                    s.master.fader = f[6].parse().map_err(|_| "bad master fader")?;
                    s.next_region_id = f[7].parse().map_err(|_| "bad next id")?;
                    session = Some(s);
                }
                Some("track") => {
                    let s = session.as_mut().ok_or("track before session")?;
                    if f.len() != 8 {
                        return Err(format!("line {}: bad track line", lineno + 1));
                    }
                    let i: usize = f[1].parse().map_err(|_| "bad track index")?;
                    if i >= N_TRACKS {
                        return Err("track index out of range".to_string());
                    }
                    let t = &mut s.tracks[i];
                    t.name = unescape(&f[2]);
                    t.input = InputSel::decode(&f[3]).ok_or("bad input")?;
                    t.armed = f[4] == "1";
                    t.mute = f[5] == "1";
                    t.solo = f[6] == "1";
                    // The strip block is one pipe field, space-separated.
                    let parts: Vec<&str> = f[7].split_whitespace().collect();
                    if parts.len() != 13 {
                        return Err(format!("line {}: bad strip block", lineno + 1));
                    }
                    let p = |idx: usize| -> Result<f32, String> {
                        parts[idx]
                            .parse()
                            .map_err(|_| "bad strip value".to_string())
                    };
                    t.strip = StripParams {
                        trim_db: p(0)?,
                        polarity: parts[1] == "1",
                        hpf_freq: p(2)?,
                        lf_gain: p(3)?,
                        lmf_gain: p(4)?,
                        lmf_freq: p(5)?,
                        hmf_gain: p(6)?,
                        hmf_freq: p(7)?,
                        hf_gain: p(8)?,
                        comp_threshold_db: p(9)?,
                        comp_ratio: p(10)?,
                        pan: p(11)?,
                        fader: p(12)?,
                    };
                }
                Some("region") => {
                    let s = session.as_mut().ok_or("region before session")?;
                    if f.len() != 7 {
                        return Err(format!("line {}: bad region line", lineno + 1));
                    }
                    let i: usize = f[1].parse().map_err(|_| "bad region track")?;
                    if i >= N_TRACKS {
                        return Err("region track out of range".to_string());
                    }
                    s.tracks[i].regions.push(Region {
                        id: f[2].parse().map_err(|_| "bad region id")?,
                        file: unescape(&f[3]),
                        start: f[4].parse().map_err(|_| "bad region start")?,
                        offset: f[5].parse().map_err(|_| "bad region offset")?,
                        len: f[6].parse().map_err(|_| "bad region len")?,
                    });
                }
                _ => return Err(format!("line {}: unknown record", lineno + 1)),
            }
        }
        session.ok_or_else(|| "no session record".to_string())
    }

    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir.join(TAKES_DIR))?;
        std::fs::write(dir.join(SESSION_FILE), self.to_text())
    }

    pub fn load_from(dir: &Path) -> Result<Session, String> {
        let text = std::fs::read_to_string(dir.join(SESSION_FILE))
            .map_err(|e| format!("cannot read session: {e}"))?;
        Session::from_text(&text)
    }

    pub fn take_path(dir: &Path, file: &str) -> PathBuf {
        dir.join(TAKES_DIR).join(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_move_trim_delete() {
        let mut s = Session::new("test", 48000);
        let id = s.add_region(0, "take-0.wav", 1000, 0, 2000);
        // Split at 2000 -> left [1000,2000), right [2000,3000).
        let right = s.split_region(0, id, 2000).unwrap();
        assert_eq!(s.find_region(0, id).unwrap().len, 1000);
        let r = s.find_region(0, right).unwrap();
        assert_eq!((r.start, r.offset, r.len), (2000, 1000, 1000));
        // Split outside the region fails.
        assert!(s.split_region(0, id, 1000).is_none());
        assert!(s.split_region(0, id, 5000).is_none());
        // Move the right half to track 1 at 5000.
        assert!(s.move_region(0, right, 1, 5000));
        assert!(s.find_region(0, right).is_none());
        assert_eq!(s.find_region(1, right).unwrap().start, 5000);
        // Trim head then tail.
        assert!(s.trim_region_start(1, right, 5500));
        let r = s.find_region(1, right).unwrap();
        assert_eq!((r.start, r.offset, r.len), (5500, 1500, 500));
        assert!(s.trim_region_end(1, right, 5800));
        assert_eq!(s.find_region(1, right).unwrap().len, 300);
        // region_at.
        assert_eq!(s.region_at(1, 5600).unwrap().id, right);
        assert!(s.region_at(1, 6000).is_none());
        // Delete.
        assert!(s.delete_region(1, right));
        assert!(!s.delete_region(1, right));
    }

    #[test]
    fn session_round_trip_with_nasty_strings() {
        let mut s = Session::new("my | session \\ test", 48000);
        s.playhead = 123456;
        s.master.maximiser = 0.75;
        s.tracks[2].name = "Vox | lead\\".to_string();
        s.tracks[2].input = InputSel::Device("Monitor of Built-in Audio | Analog".to_string());
        s.tracks[2].armed = true;
        s.tracks[2].strip.lf_gain = 6.5;
        s.tracks[2].strip.pan = -0.25;
        s.tracks[5].input = InputSel::Tone;
        s.tracks[5].solo = true;
        s.tracks[6].input =
            InputSel::PipeWire("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor".to_string());
        let _ = s.add_region(2, "take-2-123.wav", 48000, 0, 96000);
        let _ = s.add_region(2, "take-2-456.wav", 200000, 4800, 48000);
        let text = s.to_text();
        let s2 = Session::from_text(&text).unwrap();
        assert_eq!(s2.name, s.name);
        assert_eq!(s2.playhead, 123456);
        assert_eq!(s2.master, s.master);
        assert_eq!(s2.tracks[2].name, "Vox | lead\\");
        assert_eq!(
            s2.tracks[2].input,
            InputSel::Device("Monitor of Built-in Audio | Analog".to_string())
        );
        assert!(s2.tracks[2].armed);
        assert_eq!(s2.tracks[2].strip, s.tracks[2].strip);
        assert_eq!(s2.tracks[2].regions.len(), 2);
        assert_eq!(s2.tracks[2].regions[1].offset, 4800);
        assert_eq!(s2.tracks[5].input, InputSel::Tone);
        assert!(s2.tracks[5].solo);
        assert_eq!(
            s2.tracks[6].input,
            InputSel::PipeWire("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor".to_string())
        );
        // Re-serializing is stable.
        assert_eq!(s2.to_text(), text);
    }

    #[test]
    fn save_and_load_from_disk() {
        let dir = std::env::temp_dir().join("console8rs-session-test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new("disk", 48000);
        let _ = s.add_region(0, "a.wav", 0, 0, 100);
        s.save_to(&dir).unwrap();
        let s2 = Session::load_from(&dir).unwrap();
        assert_eq!(s2.name, "disk");
        assert_eq!(s2.tracks[0].regions.len(), 1);
        assert!(dir.join(TAKES_DIR).is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
