//! SDL2 interface for console-8-rs: transport bar, splice-and-arrange
//! timeline, 8 SSL-style channel strips, master bus, footer.
//! Also the headless modes: --list-devices, --smoke, --render, --shot.
//!
//! Layout (1440x900): transport 0..56, timeline 56..456 (ruler +
//! 8 lanes), strips 456..856, footer 856..900. Strips are 155 px
//! wide; the master strip takes the last 200 px.

use crate::audio::{list_input_devices, list_pw_sources, AudioSystem};
use crate::dsp::StripParams;
use crate::engine::Engine;
use crate::font::{draw_text, text_width};
use crate::session::{InputSel, Session, N_TRACKS};
use crate::wav::WavWriter;
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, RenderTarget};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const W: i32 = 1440;
const H: i32 = 900;
const GX: i32 = 200; // timeline gutter width
const STRIP_W: i32 = 155;
const MASTER_X: i32 = 1240;

const BG: Color = Color::RGB(24, 24, 28);
const PANEL: Color = Color::RGB(36, 36, 44);
const LANE_ALT: Color = Color::RGB(30, 30, 38);
const INK: Color = Color::RGB(220, 220, 220);
const DIM: Color = Color::RGB(140, 140, 150);
const PEACH: Color = Color::RGB(216, 138, 104);
const PEACH_DIM: Color = Color::RGB(120, 80, 60);
const KNOB: Color = Color::RGB(60, 60, 72);
const KNOB_MARK: Color = Color::RGB(230, 230, 230);
const BTN: Color = Color::RGB(48, 48, 58);
const BTN_ON: Color = Color::RGB(150, 95, 65);
const TRACK_COLORS: [Color; N_TRACKS] = [
    Color::RGB(90, 130, 160),
    Color::RGB(120, 150, 90),
    Color::RGB(170, 130, 90),
    Color::RGB(140, 110, 160),
    Color::RGB(90, 160, 150),
    Color::RGB(170, 100, 110),
    Color::RGB(110, 140, 190),
    Color::RGB(150, 150, 100),
];

fn fill<T: RenderTarget>(canvas: &mut Canvas<T>, x: i32, y: i32, w: u32, h: u32, c: Color) {
    canvas.set_draw_color(c);
    let _ = canvas.fill_rect(Rect::new(x, y, w, h));
}

fn rect<T: RenderTarget>(canvas: &mut Canvas<T>, x: i32, y: i32, w: u32, h: u32, c: Color) {
    canvas.set_draw_color(c);
    let _ = canvas.draw_rect(Rect::new(x, y, w, h));
}

fn line<T: RenderTarget>(canvas: &mut Canvas<T>, x1: i32, y1: i32, x2: i32, y2: i32, c: Color) {
    canvas.set_draw_color(c);
    let _ = canvas.draw_line((x1, y1), (x2, y2));
}

fn circle<T: RenderTarget>(canvas: &mut Canvas<T>, cx: i32, cy: i32, r: i32, c: Color) {
    canvas.set_draw_color(c);
    for dy in -r..=r {
        let dx = ((r * r - dy * dy) as f32).sqrt() as i32;
        let _ = canvas.fill_rect(Rect::new(cx - dx, cy + dy, (dx * 2 + 1) as u32, 1));
    }
}

fn text<T: RenderTarget>(canvas: &mut Canvas<T>, x: i32, y: i32, s: &str, scale: u32, c: Color) {
    draw_text(canvas, x, y, s, scale, c);
}

fn text_centered<T: RenderTarget>(
    canvas: &mut Canvas<T>,
    cx: i32,
    y: i32,
    s: &str,
    scale: u32,
    c: Color,
) {
    let w = text_width(s, scale);
    draw_text(canvas, cx - w / 2, y, s, scale, c);
}

// ---------------------------------------------------------------- params

#[derive(Clone, Copy, PartialEq)]
enum StripField {
    Trim,
    Hpf,
    Lf,
    LmfGain,
    LmfFreq,
    HmfGain,
    HmfFreq,
    Hf,
    Thr,
    Ratio,
    Pan,
    Fader,
}

impl StripField {
    fn range(&self) -> (f32, f32) {
        match self {
            StripField::Trim => (-24.0, 24.0),
            StripField::Hpf => (20.0, 400.0),
            StripField::Lf => (-15.0, 15.0),
            StripField::LmfGain => (-15.0, 15.0),
            StripField::LmfFreq => (200.0, 4000.0),
            StripField::HmfGain => (-15.0, 15.0),
            StripField::HmfFreq => (1000.0, 12000.0),
            StripField::Hf => (-15.0, 15.0),
            StripField::Thr => (-60.0, 0.0),
            StripField::Ratio => (1.0, 20.0),
            StripField::Pan => (-1.0, 1.0),
            StripField::Fader => (0.0, 1.25),
        }
    }

    fn log(&self) -> bool {
        matches!(
            self,
            StripField::Hpf | StripField::LmfFreq | StripField::HmfFreq
        )
    }

    fn label(&self) -> &'static str {
        match self {
            StripField::Trim => "TRIM",
            StripField::Hpf => "HPF",
            StripField::Lf => "LF",
            StripField::LmfGain => "LMF",
            StripField::LmfFreq => "LMF-F",
            StripField::HmfGain => "HMF",
            StripField::HmfFreq => "HMF-F",
            StripField::Hf => "HF",
            StripField::Thr => "THR",
            StripField::Ratio => "RATIO",
            StripField::Pan => "PAN",
            StripField::Fader => "FADER",
        }
    }

    fn get(&self, p: &StripParams) -> f32 {
        match self {
            StripField::Trim => p.trim_db,
            StripField::Hpf => p.hpf_freq,
            StripField::Lf => p.lf_gain,
            StripField::LmfGain => p.lmf_gain,
            StripField::LmfFreq => p.lmf_freq,
            StripField::HmfGain => p.hmf_gain,
            StripField::HmfFreq => p.hmf_freq,
            StripField::Hf => p.hf_gain,
            StripField::Thr => p.comp_threshold_db,
            StripField::Ratio => p.comp_ratio,
            StripField::Pan => p.pan,
            StripField::Fader => p.fader,
        }
    }

    fn set(&self, p: &mut StripParams, v: f32) {
        match self {
            StripField::Trim => p.trim_db = v,
            StripField::Hpf => p.hpf_freq = v,
            StripField::Lf => p.lf_gain = v,
            StripField::LmfGain => p.lmf_gain = v,
            StripField::LmfFreq => p.lmf_freq = v,
            StripField::HmfGain => p.hmf_gain = v,
            StripField::HmfFreq => p.hmf_freq = v,
            StripField::Hf => p.hf_gain = v,
            StripField::Thr => p.comp_threshold_db = v,
            StripField::Ratio => p.comp_ratio = v,
            StripField::Pan => p.pan = v,
            StripField::Fader => p.fader = v,
        }
    }

    fn fmt(&self, v: f32) -> String {
        match self {
            StripField::Trim
            | StripField::Lf
            | StripField::LmfGain
            | StripField::HmfGain
            | StripField::Hf => format!("{v:.1}"),
            StripField::Hpf | StripField::LmfFreq | StripField::HmfFreq => format!("{v:.0}"),
            StripField::Thr => format!("{v:.0}"),
            StripField::Ratio => format!("{v:.1}"),
            StripField::Pan => {
                if v.abs() < 0.05 {
                    "C".to_string()
                } else if v < 0.0 {
                    format!("L{:.0}", -v * 100.0)
                } else {
                    format!("R{:.0}", v * 100.0)
                }
            }
            StripField::Fader => {
                if v < 0.001 {
                    "-INF".to_string()
                } else {
                    format!("{:.1}", 20.0 * v.log10())
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Param {
    Strip(usize, StripField),
    MasterMax,
    MasterFader,
}

fn param_range(p: Param) -> (f32, f32, bool) {
    match p {
        Param::Strip(_, f) => {
            let (a, b) = f.range();
            (a, b, f.log())
        }
        Param::MasterMax => (0.0, 1.0, false),
        Param::MasterFader => (0.0, 1.25, false),
    }
}

fn param_get(eng: &Engine, p: Param) -> f32 {
    match p {
        Param::Strip(i, f) => f.get(&eng.session.tracks[i].strip),
        Param::MasterMax => eng.session.master.maximiser,
        Param::MasterFader => eng.session.master.fader,
    }
}

fn param_set(eng: &mut Engine, p: Param, v: f32) {
    match p {
        Param::Strip(i, f) => f.set(&mut eng.session.tracks[i].strip, v),
        Param::MasterMax => eng.session.master.maximiser = v,
        Param::MasterFader => eng.session.master.fader = v,
    }
}

fn to_t(v: f32, min: f32, max: f32, log: bool) -> f32 {
    if log {
        (v.ln() - min.ln()) / (max.ln() - min.ln())
    } else {
        (v - min) / (max - min)
    }
}

fn from_t(t: f32, min: f32, max: f32, log: bool) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if log {
        (min.ln() + t * (max.ln() - min.ln())).exp()
    } else {
        min + t * (max - min)
    }
}

fn fmt_time(samples: u64, sr: u32) -> String {
    let cs = samples * 100 / sr.max(1) as u64;
    format!("{:02}:{:02}.{:02}", cs / 6000, (cs / 100) % 60, cs % 100)
}

fn fmt_secs(samples: u64, sr: u32) -> String {
    format!("{:.1}S", samples as f32 / sr.max(1) as f32)
}

// ---------------------------------------------------------------- ui state

enum Drag {
    Knob { param: Param, start_y: i32, start_val: f32 },
    Fader { param: Param, start_y: i32, start_val: f32 },
    RegionMove { track: usize, id: u64, grab: u64 },
    RegionTrimStart { track: usize, id: u64 },
    RegionTrimEnd { track: usize, id: u64 },
    Scrub,
}

struct Ui {
    engine: Arc<Mutex<Engine>>,
    session_dir: PathBuf,
    view_start: u64,
    view_len: u64,
    selected: Option<(usize, u64)>,
    rec_pending: bool,
    dirty: bool,
    saved_flash: u32,
    drag: Option<Drag>,
    mouse: (i32, i32),
    audio_ok: bool,
    audio_error: Option<String>,
}

impl Ui {
    fn x_of(&self, t: u64) -> i32 {
        GX + ((t.saturating_sub(self.view_start)) as f64 / self.view_len as f64
            * (W - GX) as f64) as i32
    }

    fn t_of(&self, x: i32) -> u64 {
        let frac = ((x - GX) as f64 / (W - GX) as f64).clamp(0.0, 1.0);
        self.view_start + (frac * self.view_len as f64) as u64
    }

    fn lane_y(&self, track: usize) -> i32 {
        80 + track as i32 * 47
    }
}

// Knob cell geometry: 4 columns, 3 rows per strip.
fn knob_center(strip_x: i32, col: usize, row: usize) -> (i32, i32) {
    (strip_x + 19 + col as i32 * 39, 532 + row as i32 * 54)
}

const KNOB_ROWS: [[StripField; 4]; 3] = [
    [
        StripField::Trim,
        StripField::Hpf,
        StripField::Lf,
        StripField::LmfGain,
    ],
    [
        StripField::LmfFreq,
        StripField::HmfGain,
        StripField::HmfFreq,
        StripField::Hf,
    ],
    [StripField::Thr, StripField::Ratio, StripField::Pan, StripField::Fader],
];

// ---------------------------------------------------------------- drawing

fn draw_knob<T: RenderTarget>(
    canvas: &mut Canvas<T>,
    cx: i32,
    cy: i32,
    label: &str,
    val_text: &str,
    t: f32,
) {
    text_centered(canvas, cx, cy - 30, label, 1, DIM);
    circle(canvas, cx, cy, 12, KNOB);
    let ang = -135.0f32 + t.clamp(0.0, 1.0) * 270.0;
    let rad = ang.to_radians();
    let ix = cx + (rad.sin() * 9.0) as i32;
    let iy = cy - (rad.cos() * 9.0) as i32;
    line(canvas, cx, cy, ix, iy, KNOB_MARK);
    circle(canvas, cx, cy, 2, KNOB_MARK);
    text_centered(canvas, cx, cy + 16, val_text, 1, INK);
}

fn draw_button<T: RenderTarget>(
    canvas: &mut Canvas<T>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
    on: bool,
) {
    fill(canvas, x, y, w as u32, h as u32, if on { BTN_ON } else { BTN });
    rect(canvas, x, y, w as u32, h as u32, INK);
    text_centered(canvas, x + w / 2, y + (h - 7) / 2, label, 1, INK);
}

fn draw_fader<T: RenderTarget>(
    canvas: &mut Canvas<T>,
    x: i32,
    y: i32,
    h: i32,
    v: f32,
    val_text: &str,
) {
    // Track.
    fill(canvas, x + 2, y, 4, h as u32, BTN);
    let t = (v / 1.25).clamp(0.0, 1.0);
    let hy = y + ((1.0 - t) * h as f32) as i32 - 6;
    fill(canvas, x - 12, hy, 32, 12, KNOB);
    rect(canvas, x - 12, hy, 32, 12, INK);
    text_centered(canvas, x + 2, y + h + 8, val_text, 1, INK);
}

fn draw_meter<T: RenderTarget>(canvas: &mut Canvas<T>, x: i32, y: i32, h: i32, v: f32) {
    fill(canvas, x, y, 10, h as u32, BTN);
    let fill_h = ((v.clamp(0.0, 1.0)) * h as f32) as i32;
    if fill_h > 0 {
        fill(canvas, x, y + h - fill_h, 10, fill_h as u32, INK);
    }
    if v >= 0.99 {
        fill(canvas, x, y, 10, 4, PEACH);
    }
}

fn draw_frame<T: RenderTarget>(canvas: &mut Canvas<T>, ui: &Ui, eng: &Engine) {
    fill(canvas, 0, 0, W as u32, H as u32, BG);
    let sr = eng.session.sample_rate;

    // ---- transport bar
    fill(canvas, 0, 0, W as u32, 56, PANEL);
    let rec_on = eng.recording() || ui.rec_pending;
    draw_button(canvas, 12, 10, 76, 36, "REC", rec_on);
    draw_button(canvas, 96, 10, 76, 36, "PLAY", eng.playing);
    draw_button(canvas, 180, 10, 76, 36, "STOP", !eng.playing);
    draw_button(canvas, 264, 10, 76, 36, "REW", false);
    text(
        canvas,
        360,
        16,
        &fmt_time(eng.session.playhead, sr),
        2,
        if eng.recording() { PEACH } else { INK },
    );
    let state = if eng.recording() {
        "RECORDING"
    } else if eng.playing {
        "PLAYING"
    } else {
        "STOPPED"
    };
    text(canvas, 560, 22, state, 1, DIM);
    text(canvas, 700, 22, &format!("SESSION {}", eng.session.name), 1, DIM);
    draw_button(canvas, 1080, 10, 76, 36, "SPLIT", false);
    draw_button(canvas, 1164, 10, 76, 36, "DELETE", false);
    draw_button(
        canvas,
        1248,
        10,
        76,
        36,
        if ui.dirty { "SAVE *" } else { "SAVE" },
        false,
    );

    // ---- timeline: ruler
    fill(canvas, GX, 56, (W - GX) as u32, 24, PANEL);
    let view_end = ui.view_start + ui.view_len;
    let mut sec = ui.view_start / sr as u64 * sr as u64;
    if sec < ui.view_start {
        sec += sr as u64;
    }
    while sec <= view_end {
        let x = ui.x_of(sec);
        if x >= GX {
            let big = (sec / sr as u64) % 5 == 0;
            line(canvas, x, 72, x, if big { 80 } else { 76 }, DIM);
            if big {
                text(canvas, x + 2, 58, &format!("{}", sec / sr as u64), 1, DIM);
            }
        }
        sec += sr as u64;
    }

    // ---- lanes + headers
    for i in 0..N_TRACKS {
        let ly = ui.lane_y(i);
        let lane_bg = if i % 2 == 0 { BG } else { LANE_ALT };
        fill(canvas, GX, ly, (W - GX) as u32, 47, lane_bg);
        fill(canvas, 0, ly, GX as u32, 47, PANEL);
        line(canvas, 0, ly + 46, W, ly + 46, DIM);
        let t = &eng.session.tracks[i];
        text(canvas, 8, ly + 5, &t.name, 1, INK);
        draw_button(canvas, 118, ly + 4, 74, 20, "ARM", t.armed);
        let input_label = t.input.label();
        draw_button(canvas, 8, ly + 26, 184, 18, &input_label, false);
        // Regions.
        for r in &t.regions {
            let mut len = r.len;
            // Live length override for the take being recorded.
            if let Some((rid, frames)) = eng.live_takes.get(&i) {
                if *rid == r.id {
                    len = frames.load(std::sync::atomic::Ordering::Relaxed);
                }
            }
            let x0 = ui.x_of(r.start);
            let x1 = ui.x_of(r.start + len);
            if x1 <= GX || x0 >= W || len == 0 {
                continue;
            }
            let cx0 = x0.max(GX);
            let cx1 = x1.min(W);
            let selected = ui.selected == Some((i, r.id));
            let live = eng.live_takes.get(&i).map(|(rid, _)| *rid == r.id).unwrap_or(false);
            fill(
                canvas,
                cx0,
                ly + 6,
                (cx1 - cx0) as u32,
                35,
                TRACK_COLORS[i],
            );
            if selected || live {
                rect(canvas, cx0, ly + 6, (cx1 - cx0) as u32, 35, PEACH);
            }
            let mut stem = r.file.strip_suffix(".wav").unwrap_or(&r.file).to_string();
            if live {
                stem = format!("REC {stem}");
            }
            let max_chars = ((cx1 - cx0 - 8) / 6).max(0) as usize;
            if stem.len() > max_chars && max_chars > 0 {
                stem.truncate(max_chars);
            }
            if !stem.is_empty() && cx1 - cx0 > 20 {
                text(canvas, cx0 + 4, ly + 18, &stem, 1, INK);
            }
        }
    }

    // ---- playhead
    let px = ui.x_of(eng.session.playhead);
    if px >= GX && px < W {
        line(canvas, px, 56, px, 456, PEACH);
        line(canvas, px + 1, 56, px + 1, 456, PEACH);
    }

    // ---- strips
    for i in 0..N_TRACKS {
        let sx = i as i32 * STRIP_W;
        if i > 0 {
            line(canvas, sx, 456, sx, 856, DIM);
        }
        let t = &eng.session.tracks[i];
        text_centered(canvas, sx + STRIP_W / 2, 462, &t.name, 1, INK);
        draw_button(canvas, sx + 8, 484, 64, 22, "MUTE", t.mute);
        draw_button(canvas, sx + 83, 484, 64, 22, "SOLO", t.solo);
        for (row, fields) in KNOB_ROWS.iter().enumerate() {
            for (col, f) in fields.iter().enumerate() {
                if row == 2 && col == 3 {
                    // Polarity button in place of the 4th knob.
                    let (cx, cy) = knob_center(sx, col, row);
                    draw_button(
                        canvas,
                        cx - 15,
                        cy - 12,
                        30,
                        24,
                        "POL",
                        t.strip.polarity,
                    );
                    continue;
                }
                let (cx, cy) = knob_center(sx, col, row);
                let v = f.get(&t.strip);
                let (min, max, log) = {
                    let (a, b) = f.range();
                    (a, b, f.log())
                };
                draw_knob(canvas, cx, cy, f.label(), &f.fmt(v), to_t(v, min, max, log));
            }
        }
        // Fader + meter.
        let fv = StripField::Fader.get(&t.strip);
        draw_fader(
            canvas,
            sx + 40,
            676,
            136,
            fv,
            &StripField::Fader.fmt(fv),
        );
        draw_meter(canvas, sx + 104, 676, 136, eng.meter(i));
    }

    // ---- master strip
    line(canvas, MASTER_X, 456, MASTER_X, 856, DIM);
    text_centered(canvas, MASTER_X + 100, 462, "MASTER", 1, INK);
    fill(canvas, MASTER_X + 12, 486, 176, 30, PEACH_DIM);
    rect(canvas, MASTER_X + 12, 486, 176, 30, INK);
    text_centered(canvas, MASTER_X + 100, 495, "TUBE 1.5% ALWAYS ON", 1, INK);
    let max = eng.session.master.maximiser;
    draw_knob(
        canvas,
        MASTER_X + 100,
        560,
        "MAX",
        &format!("{:.0}%", max * 100.0),
        max,
    );
    fill(canvas, MASTER_X + 12, 606, 176, 30, BTN);
    rect(canvas, MASTER_X + 12, 606, 176, 30, INK);
    text_centered(canvas, MASTER_X + 100, 615, "LIMIT -0.5DB", 1, INK);
    draw_fader(
        canvas,
        MASTER_X + 40,
        660,
        136,
        eng.session.master.fader,
        &StripField::Fader.fmt(eng.session.master.fader),
    );
    let (ml, mr) = eng.master_meters();
    draw_meter(canvas, MASTER_X + 96, 660, 136, ml);
    draw_meter(canvas, MASTER_X + 112, 660, 136, mr);

    // ---- footer
    fill(canvas, 0, 856, W as u32, 44, PANEL);
    let mut status = format!(
        "{} {}",
        state,
        fmt_time(eng.session.playhead, sr)
    );
    let armed: Vec<String> = (0..N_TRACKS)
        .filter(|&i| eng.session.tracks[i].armed)
        .map(|i| format!("{}", i + 1))
        .collect();
    if !armed.is_empty() {
        status.push_str(&format!(" ARM {}", armed.join(" ")));
    }
    if let Some((ti, rid)) = ui.selected {
        if let Some(r) = eng.session.find_region(ti, rid) {
            status.push_str(&format!(
                " TRK {} {} {}-{}",
                ti + 1,
                r.file.strip_suffix(".wav").unwrap_or(&r.file),
                fmt_secs(r.start, sr),
                fmt_secs(r.end(), sr)
            ));
        }
    }
    if ui.saved_flash > 0 {
        status.push_str(" SAVED");
    } else if ui.dirty {
        status.push_str(" UNSAVED");
    }
    if !eng.missing_files.is_empty() {
        status.push_str(&format!(" MISSING {}", eng.missing_files[0]));
    }
    text(canvas, 12, 872, &status, 1, INK);
    let audio_text = if ui.audio_ok {
        "AUDIO OK".to_string()
    } else {
        format!(
            "AUDIO {}",
            ui.audio_error.clone().unwrap_or_else(|| "OFF".to_string())
        )
    };
    let aw = text_width(&audio_text, 1);
    text(canvas, W - 12 - aw, 872, &audio_text, 1, DIM);
    text(
        canvas,
        12,
        888 - 7,
        "SPACE PLAY/STOP  R REC  S SPLIT  DEL DELETE  CTRL S SAVE  CLICK REGION TO SELECT AND DRAG  DRAG EDGES TO TRIM",
        1,
        DIM,
    );
}

// ---------------------------------------------------------------- actions

fn save_session(ui: &mut Ui, eng: &Engine) {
    match eng.session.save_to(&ui.session_dir) {
        Ok(()) => {
            ui.dirty = false;
            ui.saved_flash = 150;
        }
        Err(e) => {
            ui.audio_error = Some(format!("save failed: {e}"));
        }
    }
}

/// Armed device tracks of an engine the caller already holds.
/// do_play/do_rec build the wanted map from `eng` and reconcile via
/// ensure_inputs_for: calling ensure_inputs here would re-lock the
/// engine guard the caller holds and deadlock the UI thread.
fn wanted_from(eng: &Engine) -> HashMap<String, Vec<usize>> {
    let mut wanted: HashMap<String, Vec<usize>> = HashMap::new();
    for i in 0..N_TRACKS {
        let t = &eng.session.tracks[i];
        if !t.armed {
            continue;
        }
        if let InputSel::Device(name) = &t.input {
            wanted.entry(name.clone()).or_default().push(i);
        }
    }
    wanted
}

/// Armed PipeWire node names of an engine the caller already holds
/// (same no-re-lock rule as wanted_from).
fn wanted_pw_from(eng: &Engine) -> HashMap<String, Vec<usize>> {
    let mut wanted: HashMap<String, Vec<usize>> = HashMap::new();
    for i in 0..N_TRACKS {
        let t = &eng.session.tracks[i];
        if !t.armed {
            continue;
        }
        if let InputSel::PipeWire(name) = &t.input {
            wanted.entry(name.clone()).or_default().push(i);
        }
    }
    wanted
}

fn do_play(ui: &mut Ui, eng: &mut Engine, audio: &mut AudioSystem) {
    if !eng.playing {
        eng.playing = true;
        if ui.rec_pending && !eng.recording() {
            eng.begin_recording();
            let wanted = wanted_from(eng);
            audio.ensure_inputs_for(&wanted);
            let pw_wanted = wanted_pw_from(eng);
            audio.ensure_pw_inputs_for(&pw_wanted);
        }
    }
}

fn do_stop(ui: &mut Ui, eng: &mut Engine, _audio: &mut AudioSystem) {
    if eng.recording() {
        eng.end_recording();
    }
    eng.playing = false;
    ui.rec_pending = false;
    let eng_ref: &Engine = eng;
    save_session(ui, eng_ref);
}

fn do_rec(ui: &mut Ui, eng: &mut Engine, audio: &mut AudioSystem) {
    if eng.playing {
        if eng.recording() {
            eng.end_recording();
        } else {
            eng.begin_recording();
            let wanted = wanted_from(eng);
            audio.ensure_inputs_for(&wanted);
            let pw_wanted = wanted_pw_from(eng);
            audio.ensure_pw_inputs_for(&pw_wanted);
        }
    } else {
        ui.rec_pending = !ui.rec_pending;
    }
}

fn cycle_input(current: &InputSel, devices: &[String], pw: &[String]) -> InputSel {
    // Cycle: None -> Tone -> ALSA devices... -> PipeWire sources... -> None.
    let mut order: Vec<InputSel> = vec![InputSel::None, InputSel::Tone];
    for d in devices {
        order.push(InputSel::Device(d.clone()));
    }
    for n in pw {
        order.push(InputSel::PipeWire(n.clone()));
    }
    let pos = order.iter().position(|x| x == current).unwrap_or(0);
    order[(pos + 1) % order.len()].clone()
}

// ---------------------------------------------------------------- headless

fn demo_engine() -> (Engine, PathBuf) {
    let dir = std::env::temp_dir().join("console8rs-demo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("takes")).unwrap();
    let mk = |file: &str, freq: f32, amp: f32| {
        let path = Session::take_path(&dir, file);
        let mut w = WavWriter::create(&path, 48000).unwrap();
        let frames: Vec<f32> = (0..480000)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin())
            .collect();
        w.write(&frames).unwrap();
        w.finalize().unwrap();
    };
    mk("a.wav", 440.0, 0.5);
    mk("b.wav", 220.0, 0.3);
    let mut s = Session::new("DEMO", 48000);
    let _ = s.add_region(0, "a.wav", 0, 0, 480000);
    let _ = s.add_region(1, "b.wav", 96000, 0, 384000);
    s.tracks[0].strip.lf_gain = 3.0;
    s.tracks[1].strip.pan = -0.3;
    s.tracks[1].strip.fader = 0.8;
    (Engine::new(s, dir.clone()), dir)
}

fn render_mix(eng: &mut Engine, secs: f32) -> (Vec<f32>, Vec<f32>) {
    eng.playing = true;
    let total = (secs * eng.session.sample_rate as f32) as usize;
    let mut l_all = Vec::with_capacity(total);
    let mut r_all = Vec::with_capacity(total);
    let empty: [&[f32]; N_TRACKS] = [&[], &[], &[], &[], &[], &[], &[], &[]];
    let mut done = 0;
    while done < total {
        let n = (total - done).min(512);
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        eng.render_block(&mut l, &mut r, &empty);
        l_all.extend_from_slice(&l);
        r_all.extend_from_slice(&r);
        done += n;
    }
    (l_all, r_all)
}

fn smoke(secs: f32) -> i32 {
    let (mut eng, dir) = demo_engine();
    let (l, r) = render_mix(&mut eng, secs);
    let peak = l
        .iter()
        .chain(r.iter())
        .fold(0.0f32, |m, x| m.max(x.abs()));
    let rms = (l.iter().map(|x| x * x).sum::<f32>() / l.len() as f32).sqrt();
    let finite = l.iter().chain(r.iter()).all(|x| x.is_finite());
    println!(
        "smoke: {} samples peak {:.3} rms {:.3} finite {} playhead {}",
        l.len(),
        peak,
        rms,
        finite,
        eng.session.playhead
    );
    let _ = std::fs::remove_dir_all(&dir);
    if finite && peak > 0.05 && peak <= 1.0 {
        println!("SMOKE OK");
        0
    } else {
        println!("SMOKE FAILED");
        1
    }
}

fn write_stereo_wav(path: &Path, sr: u32, l: &[f32], r: &[f32]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let n = l.len().min(r.len()) as u32;
    let data_bytes = n * 8;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&3u16.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&(sr * 8).to_le_bytes())?;
    f.write_all(&8u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    for i in 0..n as usize {
        f.write_all(&l[i].to_le_bytes())?;
        f.write_all(&r[i].to_le_bytes())?;
    }
    f.flush()
}

fn render_wav(out: &str, secs: f32) -> i32 {
    let (mut eng, dir) = demo_engine();
    let (l, r) = render_mix(&mut eng, secs);
    match write_stereo_wav(Path::new(out), 48000, &l, &r) {
        Ok(()) => {
            println!("rendered {} samples to {}", l.len(), out);
            let _ = std::fs::remove_dir_all(&dir);
            0
        }
        Err(e) => {
            eprintln!("render failed: {e}");
            1
        }
    }
}

fn shot(bmp: &str) -> i32 {
    std::env::set_var("SDL_VIDEODRIVER", "dummy");
    let sdl = match sdl2::init() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SDL init failed: {e}");
            return 1;
        }
    };
    let video = match sdl.video() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SDL video failed: {e}");
            return 1;
        }
    };
    let _video = video;
    let surface = match sdl2::surface::Surface::new(W as u32, H as u32, sdl2::pixels::PixelFormatEnum::RGB24) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("surface failed: {e}");
            return 1;
        }
    };
    let mut canvas = match surface.into_canvas() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("canvas failed: {e}");
            return 1;
        }
    };
    let (eng, dir) = demo_engine();
    let ui = Ui {
        engine: Arc::new(Mutex::new(Engine::new(
            Session::new("DEMO", 48000),
            dir.clone(),
        ))),
        session_dir: dir.clone(),
        view_start: 0,
        view_len: 60 * 48000,
        selected: Some((0, 1)),
        rec_pending: false,
        dirty: false,
        saved_flash: 0,
        drag: None,
        mouse: (0, 0),
        audio_ok: false,
        audio_error: None,
    };
    // Draw with the demo engine (regions present) but the demo Ui's
    // engine field is a placeholder; draw_frame takes eng directly.
    draw_frame(&mut canvas, &ui, &eng);
    let surface = canvas.into_surface();
    match surface.save_bmp(Path::new(bmp)) {
        Ok(()) => {
            println!("shot saved to {}", bmp);
            let _ = std::fs::remove_dir_all(&dir);
            0
        }
        Err(e) => {
            eprintln!("shot save failed: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------- live ui

fn live(session_dir: PathBuf) -> i32 {
    let session_path = session_dir.join(crate::session::SESSION_FILE);
    let session = match Session::load_from(&session_dir) {
        Ok(s) => s,
        Err(_) => {
            // Never overwrite an unreadable session file blindly:
            // back it up once so a later save can't destroy the
            // only copy of a session that might be hand-repairable.
            if session_path.exists() {
                let bak = session_dir.join("session.txt.bak");
                if !bak.exists() {
                    let _ = std::fs::copy(&session_path, &bak);
                }
            }
            Session::new("DEFAULT", 48000)
        }
    };
    let sr = session.sample_rate;
    let engine = Arc::new(Mutex::new(Engine::new(session, session_dir.clone())));
    let mut audio = AudioSystem::new(engine.clone(), sr);
    audio.start_output();
    audio.ensure_inputs();
    audio.ensure_pw_inputs();

    let sdl = match sdl2::init() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SDL init failed: {e}");
            return 1;
        }
    };
    let video = match sdl.video() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SDL video failed: {e}");
            return 1;
        }
    };
    let window = match video
        .window("CONSOLE-8-RS", W as u32, H as u32)
        .position_centered()
        .build()
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("window failed: {e}");
            return 1;
        }
    };
    let mut canvas = match window.into_canvas().present_vsync().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("canvas failed: {e}");
            return 1;
        }
    };
    let mut events = sdl.event_pump().unwrap();
    let mut ui = Ui {
        engine: engine.clone(),
        session_dir: session_dir.clone(),
        view_start: 0,
        view_len: 60 * sr as u64,
        selected: None,
        rec_pending: false,
        dirty: false,
        saved_flash: 0,
        drag: None,
        mouse: (0, 0),
        audio_ok: audio.output_ok(),
        audio_error: audio.last_error.clone(),
    };
    // Sync view to a loaded playhead.
    {
        let eng = engine.lock().unwrap();
        if eng.session.playhead >= ui.view_start + ui.view_len {
            ui.view_start = eng.session.playhead - ui.view_len * 3 / 4;
        }
    }

    'main: loop {
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. } => {
                    let mut eng = engine.lock().unwrap();
                    if eng.recording() {
                        eng.end_recording();
                    }
                    eng.playing = false;
                    save_session(&mut ui, &eng);
                    break 'main;
                }
                Event::MouseMotion { x, y, .. } => {
                    ui.mouse = (x, y);
                    handle_drag(&mut ui, &mut audio);
                }
                Event::MouseButtonDown {
                    x, y, mouse_btn, ..
                } => {
                    if mouse_btn == MouseButton::Left {
                        ui.mouse = (x, y);
                        handle_mousedown(&mut ui, &mut audio, x, y);
                    }
                }
                Event::MouseButtonUp {
                    mouse_btn, ..
                } => {
                    if mouse_btn == MouseButton::Left {
                        if matches!(
                            ui.drag,
                            Some(Drag::RegionMove { .. })
                                | Some(Drag::RegionTrimStart { .. })
                                | Some(Drag::RegionTrimEnd { .. })
                        ) {
                            ui.dirty = true;
                        }
                        if matches!(ui.drag, Some(Drag::Knob { .. }) | Some(Drag::Fader { .. })) {
                            ui.dirty = true;
                        }
                        ui.drag = None;
                    }
                }
                Event::MouseWheel { y: scroll, .. } => {
                    handle_wheel(&mut ui, scroll);
                }
                Event::KeyDown {
                    keycode: Some(k),
                    keymod,
                    ..
                } => {
                    let ctrl = keymod.contains(sdl2::keyboard::Mod::LCTRLMOD)
                        || keymod.contains(sdl2::keyboard::Mod::RCTRLMOD);
                    let shift = keymod.contains(sdl2::keyboard::Mod::LSHIFTMOD)
                        || keymod.contains(sdl2::keyboard::Mod::RSHIFTMOD);
                    match k {
                        Keycode::Space => {
                            let mut eng = engine.lock().unwrap();
                            if eng.playing {
                                do_stop(&mut ui, &mut eng, &mut audio);
                            } else {
                                do_play(&mut ui, &mut eng, &mut audio);
                            }
                        }
                        Keycode::R => {
                            let mut eng = engine.lock().unwrap();
                            do_rec(&mut ui, &mut eng, &mut audio);
                        }
                        Keycode::S => {
                            if ctrl {
                                let eng = engine.lock().unwrap();
                                save_session(&mut ui, &eng);
                            } else {
                                split_selected(&mut ui);
                            }
                        }
                        Keycode::Delete | Keycode::Backspace => {
                            delete_selected(&mut ui);
                        }
                        Keycode::Escape => {
                            ui.selected = None;
                        }
                        Keycode::Left | Keycode::Right => {
                            let mut eng = engine.lock().unwrap();
                            let step = if shift { 10 } else { 1 } * sr as u64;
                            if k == Keycode::Left {
                                eng.session.playhead =
                                    eng.session.playhead.saturating_sub(step);
                            } else {
                                eng.session.playhead += step;
                            }
                            if eng.session.playhead < ui.view_start
                                || eng.session.playhead >= ui.view_start + ui.view_len
                            {
                                ui.view_start = eng
                                    .session
                                    .playhead
                                    .saturating_sub(ui.view_len / 2);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // Edge-scroll while dragging regions.
        if matches!(
            ui.drag,
            Some(Drag::RegionMove { .. })
                | Some(Drag::RegionTrimStart { .. })
                | Some(Drag::RegionTrimEnd { .. })
        ) {
            let (mx, _) = ui.mouse;
            if mx > W - 12 {
                ui.view_start += ui.view_len / 240;
                handle_drag(&mut ui, &mut audio);
            } else if mx < GX + 12 && ui.view_start > 0 {
                ui.view_start = ui.view_start.saturating_sub(ui.view_len / 240);
                handle_drag(&mut ui, &mut audio);
            }
        }
        // Follow the playhead while playing.
        {
            let eng = engine.lock().unwrap();
            if eng.playing && eng.session.playhead >= ui.view_start + ui.view_len {
                ui.view_start = eng.session.playhead - ui.view_len * 3 / 4;
            }
            if ui.saved_flash > 0 {
                ui.saved_flash -= 1;
            }
            ui.audio_ok = audio.output_ok();
            audio.poll_pw_error();
            ui.audio_error = audio.last_error.clone();
            draw_frame(&mut canvas, &ui, &eng);
        }
        canvas.present();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    0
}

fn split_selected(ui: &mut Ui) {
    let (ti, rid) = match ui.selected {
        Some(s) => s,
        None => return,
    };
    let mut eng = ui.engine.lock().unwrap();
    let playhead = eng.session.playhead;
    if eng.session.split_region(ti, rid, playhead).is_some() {
        eng.drop_readers();
        ui.dirty = true;
    }
}

fn delete_selected(ui: &mut Ui) {
    let (ti, rid) = match ui.selected {
        Some(s) => s,
        None => return,
    };
    let mut eng = ui.engine.lock().unwrap();
    if eng.session.delete_region(ti, rid) {
        eng.drop_readers();
        ui.selected = None;
        ui.dirty = true;
    }
}

fn handle_drag(ui: &mut Ui, _audio: &mut AudioSystem) {
    let (mx, my) = ui.mouse;
    let drag = match &ui.drag {
        Some(d) => d,
        None => return,
    };
    let mut eng = ui.engine.lock().unwrap();
    match drag {
        Drag::Knob {
            param,
            start_y,
            start_val,
        } => {
            let (min, max, log) = param_range(*param);
            let t0 = to_t(*start_val, min, max, log);
            let t = t0 + (*start_y as f32 - my as f32) / 150.0;
            param_set(&mut eng, *param, from_t(t, min, max, log));
        }
        Drag::Fader {
            param,
            start_y,
            start_val,
        } => {
            let (min, max, _) = param_range(*param);
            let v = *start_val + (*start_y as f32 - my as f32) / 136.0 * (max - min);
            param_set(&mut eng, *param, v.clamp(min, max));
        }
        Drag::RegionMove { track, id, grab } => {
            let t = ui.t_of(mx);
            let new_start = t.saturating_sub(*grab);
            let (track, id) = (*track, *id);
            eng.session.move_region(track, id, track, new_start);
        }
        Drag::RegionTrimStart { track, id } => {
            let t = ui.t_of(mx);
            let (track, id) = (*track, *id);
            eng.session.trim_region_start(track, id, t);
        }
        Drag::RegionTrimEnd { track, id } => {
            let t = ui.t_of(mx);
            let (track, id) = (*track, *id);
            eng.session.trim_region_end(track, id, t);
        }
        Drag::Scrub => {
            eng.session.playhead = ui.t_of(mx);
            if eng.session.playhead < ui.view_start
                || eng.session.playhead >= ui.view_start + ui.view_len
            {
                ui.view_start = eng.session.playhead.saturating_sub(ui.view_len / 2);
            }
        }
    }
}

fn handle_wheel(ui: &mut Ui, scroll: i32) {
    if scroll == 0 {
        return;
    }
    let (mx, my) = ui.mouse;
    // Knobs first.
    for i in 0..N_TRACKS {
        let sx = i as i32 * STRIP_W;
        for (row, fields) in KNOB_ROWS.iter().enumerate() {
            for (col, f) in fields.iter().enumerate() {
                if row == 2 && col == 3 {
                    continue;
                }
                let (cx, cy) = knob_center(sx, col, row);
                if (mx - cx).abs() <= 17 && (my - cy).abs() <= 17 {
                    let mut eng = ui.engine.lock().unwrap();
                    let p = Param::Strip(i, *f);
                    let (min, max, log) = param_range(p);
                    let v = param_get(&eng, p);
                    let t = to_t(v, min, max, log) + scroll as f32 * 0.02;
                    param_set(&mut eng, p, from_t(t, min, max, log));
                    ui.dirty = true;
                    return;
                }
            }
        }
        // Fader.
        if mx >= sx + 24 && mx <= sx + 58 && my >= 670 && my <= 818 {
            let mut eng = ui.engine.lock().unwrap();
            let p = Param::Strip(i, StripField::Fader);
            let v = (param_get(&eng, p) + scroll as f32 * 0.02).clamp(0.0, 1.25);
            param_set(&mut eng, p, v);
            ui.dirty = true;
            return;
        }
    }
    // Master max knob / fader.
    let (cx, cy) = (MASTER_X + 100, 560);
    if (mx - cx).abs() <= 20 && (my - cy).abs() <= 20 {
        let mut eng = ui.engine.lock().unwrap();
        let v = (param_get(&eng, Param::MasterMax) + scroll as f32 * 0.02).clamp(0.0, 1.0);
        param_set(&mut eng, Param::MasterMax, v);
        ui.dirty = true;
        return;
    }
    if mx >= MASTER_X + 28 && mx <= MASTER_X + 62 && my >= 654 && my <= 806 {
        let mut eng = ui.engine.lock().unwrap();
        let v = (param_get(&eng, Param::MasterFader) + scroll as f32 * 0.02).clamp(0.0, 1.25);
        param_set(&mut eng, Param::MasterFader, v);
        ui.dirty = true;
    }
}

fn handle_mousedown(ui: &mut Ui, audio: &mut AudioSystem, x: i32, y: i32) {
    // ---- transport buttons
    let transport_btns: [(&str, i32); 7] = [
        ("REC", 12),
        ("PLAY", 96),
        ("STOP", 180),
        ("REW", 264),
        ("SPLIT", 1080),
        ("DELETE", 1164),
        ("SAVE", 1248),
    ];
    if y >= 10 && y <= 46 {
        for (name, bx) in transport_btns {
            if x >= bx && x < bx + 76 {
                // Clone the Arc first: the actions below need &mut Ui,
                // which conflicts with a guard borrowed from ui.engine.
                let engine_arc = ui.engine.clone();
                let mut eng = engine_arc.lock().unwrap();
                match name {
                    "REC" => do_rec(ui, &mut eng, audio),
                    "PLAY" => do_play(ui, &mut eng, audio),
                    "STOP" => do_stop(ui, &mut eng, audio),
                    "REW" => {
                        if !eng.recording() {
                            eng.session.playhead = 0;
                            ui.view_start = 0;
                        }
                    }
                    "SPLIT" => {
                        drop(eng);
                        split_selected(ui);
                    }
                    "DELETE" => {
                        drop(eng);
                        delete_selected(ui);
                    }
                    "SAVE" => {
                        save_session(ui, &eng);
                    }
                    _ => {}
                }
                return;
            }
        }
    }
    // ---- ruler: scrub
    if y >= 56 && y < 80 && x >= GX {
        ui.drag = Some(Drag::Scrub);
        let mut eng = ui.engine.lock().unwrap();
        eng.session.playhead = ui.t_of(x);
        return;
    }
    // ---- lanes
    if y >= 80 && y < 456 {
        let track = ((y - 80) / 47) as usize;
        if track >= N_TRACKS {
            return;
        }
        if x < GX {
            // Header: ARM + input buttons.
            let ly = ui.lane_y(track);
            if y >= ly + 4 && y < ly + 24 && x >= 118 && x < 192 {
                let mut eng = ui.engine.lock().unwrap();
                eng.session.tracks[track].armed = !eng.session.tracks[track].armed;
                ui.dirty = true;
                drop(eng);
                audio.ensure_inputs();
                audio.ensure_pw_inputs();
                return;
            }
            if y >= ly + 26 && y < ly + 44 && x >= 8 && x < 192 {
                let devices = list_input_devices();
                // PipeWire enumeration is capped at 1s so a wedged
                // daemon costs a slow button press, never a freeze.
                let pw_names: Vec<String> = match list_pw_sources(
                    std::time::Duration::from_secs(1),
                ) {
                    Ok(srcs) => srcs.iter().map(|s| s.name.clone()).collect(),
                    Err(e) => {
                        audio.last_error = Some(e);
                        Vec::new()
                    }
                };
                let mut eng = ui.engine.lock().unwrap();
                let cur = eng.session.tracks[track].input.clone();
                eng.session.tracks[track].input = cycle_input(&cur, &devices, &pw_names);
                ui.dirty = true;
                drop(eng);
                audio.ensure_inputs();
                audio.ensure_pw_inputs();
                return;
            }
            return;
        }
        // Plot: region hit or empty -> seek.
        let t = ui.t_of(x);
        let mut eng = ui.engine.lock().unwrap();
        // Regions, topmost last.
        let regions: Vec<(u64, u64, u64, u64)> = eng.session.tracks[track]
            .regions
            .iter()
            .map(|r| {
                let mut len = r.len;
                if let Some((rid, frames)) = eng.live_takes.get(&track) {
                    if *rid == r.id {
                        len = frames.load(std::sync::atomic::Ordering::Relaxed);
                    }
                }
                (r.id, r.start, r.offset, len)
            })
            .collect();
        for (id, start, _offset, len) in regions.iter().rev() {
            let x0 = ui.x_of(*start);
            let x1 = ui.x_of(*start + *len);
            if x >= x0 && x <= x1 && *len > 0 {
                ui.selected = Some((track, *id));
                if (x - x0).abs() <= 6 {
                    ui.drag = Some(Drag::RegionTrimStart { track, id: *id });
                } else if (x - x1).abs() <= 6 {
                    ui.drag = Some(Drag::RegionTrimEnd { track, id: *id });
                } else {
                    ui.drag = Some(Drag::RegionMove {
                        track,
                        id: *id,
                        grab: t.saturating_sub(*start),
                    });
                }
                return;
            }
        }
        // Empty lane: seek + clear selection.
        eng.session.playhead = t;
        ui.selected = None;
        return;
    }
    // ---- strips
    if y >= 456 && y < 856 {
        if x >= MASTER_X {
            // Master max knob.
            let (cx, cy) = (MASTER_X + 100, 560);
            if (x - cx).abs() <= 20 && (y - cy).abs() <= 20 {
                let eng = ui.engine.lock().unwrap();
                ui.drag = Some(Drag::Knob {
                    param: Param::MasterMax,
                    start_y: y,
                    start_val: param_get(&eng, Param::MasterMax),
                });
                return;
            }
            // Master fader.
            if x >= MASTER_X + 28 && x <= MASTER_X + 62 && y >= 654 && y <= 806 {
                let eng = ui.engine.lock().unwrap();
                ui.drag = Some(Drag::Fader {
                    param: Param::MasterFader,
                    start_y: y,
                    start_val: param_get(&eng, Param::MasterFader),
                });
                return;
            }
            return;
        }
        let i = (x / STRIP_W) as usize;
        if i >= N_TRACKS {
            return;
        }
        let sx = i as i32 * STRIP_W;
        // MUTE / SOLO.
        if y >= 484 && y < 506 {
            if x >= sx + 8 && x < sx + 72 {
                let mut eng = ui.engine.lock().unwrap();
                eng.session.tracks[i].mute = !eng.session.tracks[i].mute;
                ui.dirty = true;
                return;
            }
            if x >= sx + 83 && x < sx + 147 {
                let mut eng = ui.engine.lock().unwrap();
                eng.session.tracks[i].solo = !eng.session.tracks[i].solo;
                ui.dirty = true;
                return;
            }
        }
        // Knobs + polarity.
        for (row, fields) in KNOB_ROWS.iter().enumerate() {
            for (col, f) in fields.iter().enumerate() {
                let (cx, cy) = knob_center(sx, col, row);
                if (x - cx).abs() <= 17 && (y - cy).abs() <= 17 {
                    if row == 2 && col == 3 {
                        let mut eng = ui.engine.lock().unwrap();
                        eng.session.tracks[i].strip.polarity =
                            !eng.session.tracks[i].strip.polarity;
                        ui.dirty = true;
                        return;
                    }
                    let eng = ui.engine.lock().unwrap();
                    let p = Param::Strip(i, *f);
                    ui.drag = Some(Drag::Knob {
                        param: p,
                        start_y: y,
                        start_val: param_get(&eng, p),
                    });
                    return;
                }
            }
        }
        // Fader.
        if x >= sx + 24 && x <= sx + 58 && y >= 670 && y <= 818 {
            let eng = ui.engine.lock().unwrap();
            let p = Param::Strip(i, StripField::Fader);
            ui.drag = Some(Drag::Fader {
                param: p,
                start_y: y,
                start_val: param_get(&eng, p),
            });
            return;
        }
    }
}

// ---------------------------------------------------------------- entry

fn print_help() {
    println!(
        "console-8-rs: 8-track tape recorder and mixer\n\
         \n\
         usage: console-8-rs [options]\n\
         \n\
         --session <dir>    session dir (default: sessions/default)\n\
         --list-devices     list cpal input devices and PipeWire sources and exit\n\
         --smoke [secs]     render a synthetic mix headless, exit 0/1\n\
         --render <out.wav> [secs]  render a synthetic mix to a stereo WAV\n\
         --shot <out.bmp>   render one UI frame to a BMP (dummy video)\n\
         --help             this text"
    );
}

pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let mut session_dir = PathBuf::from("sessions/default");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" => {
                print_help();
                return 0;
            }
            "--list-devices" => {
                let devs = list_input_devices();
                if devs.is_empty() {
                    println!("no cpal input devices enumerated");
                } else {
                    for d in devs {
                        println!("{d}");
                    }
                }
                match list_pw_sources(std::time::Duration::from_secs(5)) {
                    Ok(srcs) => {
                        if srcs.is_empty() {
                            println!("no pipewire sources enumerated");
                        } else {
                            println!("pipewire sources:");
                            for s in srcs {
                                println!("  {} ({})", s.description, s.name);
                            }
                        }
                    }
                    Err(e) => println!("pipewire sources unavailable: {e}"),
                }
                return 0;
            }
            "--smoke" => {
                let secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(3.0);
                return smoke(secs);
            }
            "--render" => {
                let out = match args.get(i + 1) {
                    Some(o) => o.clone(),
                    None => {
                        eprintln!("--render needs an output path");
                        return 1;
                    }
                };
                let secs = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(5.0);
                return render_wav(&out, secs);
            }
            "--shot" => {
                let bmp = match args.get(i + 1) {
                    Some(o) => o.clone(),
                    None => {
                        eprintln!("--shot needs an output path");
                        return 1;
                    }
                };
                return shot(&bmp);
            }
            "--session" => {
                if let Some(d) = args.get(i + 1) {
                    session_dir = PathBuf::from(d);
                    i += 1;
                }
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                return 1;
            }
        }
        i += 1;
    }
    live(session_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn play_after_rec_pending_does_not_deadlock_under_guard() {
        // Regression for 2026-10-26: pressing PLAY with REC pending
        // froze the window (GNOME force-quit it) because do_play
        // called ensure_inputs() while the caller held the engine
        // guard, and ensure_inputs re-locked the same non-reentrant
        // Mutex. Run the exact sequence on a worker thread and fail
        // (don't hang the suite) if it doesn't return in 5s.
        let dir = std::env::temp_dir().join("console8rs-deadlock-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("takes")).unwrap();
        let mut s = Session::new("t", 48000);
        s.tracks[0].armed = true;
        s.tracks[0].input = InputSel::Device("no-such-device".to_string());
        let engine = Arc::new(Mutex::new(Engine::new(s, dir.clone())));
        let mut ui = Ui {
            engine: engine.clone(),
            session_dir: dir.clone(),
            view_start: 0,
            view_len: 60 * 48000,
            selected: None,
            rec_pending: true,
            dirty: false,
            saved_flash: 0,
            drag: None,
            mouse: (0, 0),
            audio_ok: false,
            audio_error: None,
        };
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        // AudioSystem is constructed inside the worker: it holds
        // cpal::Stream handles, which are not Send.
        let handle = std::thread::spawn(move || {
            let mut audio = AudioSystem::new(engine.clone(), 48000);
            let mut eng = engine.lock().unwrap();
            do_play(&mut ui, &mut eng, &mut audio);
            do_stop(&mut ui, &mut eng, &mut audio);
            done2.store(true, Ordering::SeqCst);
        });
        for _ in 0..50 {
            if done.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            done.load(Ordering::SeqCst),
            "do_play deadlocked under a held engine guard"
        );
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pw_capture_fails_gracefully_without_daemon() {
        // No PipeWire daemon runs in this VM. Arming a PipeWire node
        // and pressing PLAY must still return under the guard, and the
        // capture thread's failure must surface as a footer error,
        // never a hang or a panic.
        let dir = std::env::temp_dir().join("console8rs-pw-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("takes")).unwrap();
        let mut s = Session::new("t", 48000);
        s.tracks[0].armed = true;
        s.tracks[0].input = InputSel::PipeWire("no-such-node".to_string());
        let engine = Arc::new(Mutex::new(Engine::new(s, dir.clone())));
        let mut ui = Ui {
            engine: engine.clone(),
            session_dir: dir.clone(),
            view_start: 0,
            view_len: 60 * 48000,
            selected: None,
            rec_pending: true,
            dirty: false,
            saved_flash: 0,
            drag: None,
            mouse: (0, 0),
            audio_ok: false,
            audio_error: None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut audio = AudioSystem::new(engine.clone(), 48000);
            let mut eng = engine.lock().unwrap();
            do_play(&mut ui, &mut eng, &mut audio);
            // Give the capture thread time to fail connecting, then
            // surface its error like the live loop does each frame.
            for _ in 0..20 {
                audio.poll_pw_error();
                if audio.last_error.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            do_stop(&mut ui, &mut eng, &mut audio);
            let _ = tx.send(audio.last_error.clone());
        });
        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("pw test thread must finish");
        let _ = handle.join();
        let msg = err.expect("capture failure must surface as an error");
        assert!(
            msg.contains("pipewire"),
            "error should name pipewire, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
