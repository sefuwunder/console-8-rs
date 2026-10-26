//! cpal glue for console-8-rs: one stereo output stream rendering the
//! engine, and one input stream per device that at least one armed
//! track uses. Input sources are whatever the host exposes — a mic,
//! a monitor source (app audio on PipeWire/PulseAudio), or an ALSA
//! loopback — plus the built-in TONE test source, which needs no
//! hardware at all.
//!
//! Tradeoff, stated plainly: the engine lives behind one Mutex shared
//! by the UI and both audio callbacks. A disk stall in the render
//! callback can briefly stall input. For a self-hosted 8-track this
//! is fine; a lock-free design would be the next step if dropouts
//! ever showed up.

use crate::engine::Engine;
use crate::pw::{PwCapture, PwSource};
use crate::session::{InputSel, N_TRACKS};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleRate, Stream, StreamConfig};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct MonitorHub {
    queues: [Arc<Mutex<VecDeque<f32>>>; N_TRACKS],
}

impl MonitorHub {
    pub fn new() -> Self {
        MonitorHub {
            queues: std::array::from_fn(|_| Arc::new(Mutex::new(VecDeque::new()))),
        }
    }

    pub fn push(&self, track: usize, frames: &[f32]) {
        if let Some(q) = self.queues.get(track) {
            let mut q = q.lock().unwrap();
            q.extend(frames.iter());
            // Cap: ~2s of backlog; drop oldest if the consumer stalls.
            while q.len() > 96000 {
                q.pop_front();
            }
        }
    }

    pub fn drain(&self, track: usize, n: usize) -> Vec<f32> {
        match self.queues.get(track) {
            Some(q) => {
                let mut q = q.lock().unwrap();
                let take = n.min(q.len());
                q.drain(..take).collect()
            }
            None => Vec::new(),
        }
    }
}

pub struct AudioSystem {
    engine: Arc<Mutex<Engine>>,
    hub: Arc<MonitorHub>,
    output_stream: Option<Stream>,
    input_streams: HashMap<String, Stream>,
    /// device name -> subscribed tracks (shared with input callbacks)
    input_subs: Arc<Mutex<HashMap<String, Vec<usize>>>>,
    /// PipeWire node name -> running capture (separate from cpal: a
    /// PipeWire monitor is not an ALSA device and must never share the
    /// input_subs key space, where a name collision would cross-feed
    /// tracks).
    pw_streams: HashMap<String, PwCapture>,
    /// PipeWire node name -> subscribed tracks (shared with capture
    /// threads, same shape as input_subs).
    pw_subs: Arc<Mutex<HashMap<String, Vec<usize>>>>,
    /// Capture threads report here; poll_pw_error moves it to
    /// last_error so the footer shows it within a frame.
    pw_error: Arc<Mutex<Option<String>>>,
    sample_rate: u32,
    tone_phase: Arc<Mutex<f32>>,
    pub last_error: Option<String>,
}

/// PipeWire recordable nodes: Audio/Source nodes (mics) and
/// Audio/Sink nodes (captured via their monitor ports). The timeout
/// keeps a wedged daemon from stalling the caller.
pub fn list_pw_sources(timeout: Duration) -> Result<Vec<PwSource>, String> {
    crate::pw::list_sources_timeout(timeout)
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => {
            let mut names: Vec<String> = devs
                .filter_map(|d| d.name().ok())
                .collect();
            names.sort();
            names.dedup();
            names
        }
        Err(_) => Vec::new(),
    }
}

impl AudioSystem {
    pub fn new(engine: Arc<Mutex<Engine>>, sample_rate: u32) -> AudioSystem {
        AudioSystem {
            engine,
            hub: Arc::new(MonitorHub::new()),
            output_stream: None,
            input_streams: HashMap::new(),
            input_subs: Arc::new(Mutex::new(HashMap::new())),
            pw_streams: HashMap::new(),
            pw_subs: Arc::new(Mutex::new(HashMap::new())),
            pw_error: Arc::new(Mutex::new(None)),
            sample_rate,
            tone_phase: Arc::new(Mutex::new(0.0)),
            last_error: None,
        }
    }

    /// Move a capture-thread error into last_error (footer reads it
    /// every frame). Call from the live loop, not from under the
    /// engine guard: it only touches the error slot, never the engine.
    pub fn poll_pw_error(&mut self) {
        if let Ok(mut e) = self.pw_error.lock() {
            if let Some(msg) = e.take() {
                self.last_error = Some(msg);
            }
        }
    }

    pub fn output_ok(&self) -> bool {
        self.output_stream.is_some()
    }

    pub fn start_output(&mut self) {
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                self.last_error = Some("no output device".to_string());
                return;
            }
        };
        let config = StreamConfig {
            channels: 2,
            sample_rate: SampleRate(self.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let engine = self.engine.clone();
        let hub = self.hub.clone();
        let tone_phase = self.tone_phase.clone();
        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / 2;
                // Drain monitors + synthesize tone monitors.
                let mut mons: Vec<Vec<f32>> = Vec::with_capacity(N_TRACKS);
                for i in 0..N_TRACKS {
                    mons.push(hub.drain(i, frames));
                }
                let mut eng = match engine.lock() {
                    Ok(e) => e,
                    Err(_) => return,
                };
                // Tone test source: armed tone tracks monitor a 440 Hz
                // sine generated on the output clock.
                for i in 0..N_TRACKS {
                    if eng.session.tracks[i].armed
                        && matches!(eng.session.tracks[i].input, InputSel::Tone)
                    {
                        let mut phase = tone_phase.lock().unwrap();
                        let step =
                            2.0 * std::f32::consts::PI * 440.0 / eng.session.sample_rate as f32;
                        let mut buf = Vec::with_capacity(frames);
                        for _ in 0..frames {
                            buf.push(phase.sin() * 0.5);
                            *phase += step;
                            if *phase > 2.0 * std::f32::consts::PI {
                                *phase -= 2.0 * std::f32::consts::PI;
                            }
                        }
                        mons[i] = buf;
                    }
                }
                let refs: [&[f32]; N_TRACKS] =
                    std::array::from_fn(|i| mons[i].as_slice());
                let mut l = vec![0.0f32; frames];
                let mut r = vec![0.0f32; frames];
                eng.render_block(&mut l, &mut r, &refs);
                for j in 0..frames {
                    data[j * 2] = l[j];
                    data[j * 2 + 1] = r[j];
                }
            },
            move |err| {
                eprintln!("console-8-rs output error: {err}");
            },
            None,
        );
        match stream {
            Ok(s) => {
                if s.play().is_ok() {
                    self.output_stream = Some(s);
                    self.last_error = None;
                } else {
                    self.last_error = Some("could not start output stream".to_string());
                }
            }
            Err(e) => {
                self.last_error = Some(format!("no output stream ({e})"));
            }
        }
    }

    fn find_input_device(name: &str) -> Option<cpal::Device> {
        let host = cpal::default_host();
        if let Ok(devs) = host.input_devices() {
            for d in devs {
                if let Ok(n) = d.name() {
                    if n == name {
                        return Some(d);
                    }
                }
            }
        }
        None
    }

    /// Reconcile input streams with the engine's armed tracks.
    /// Safe to call every frame; only starts/stops on change.
    /// Armed device tracks, read under the engine lock.
    fn wanted_inputs(&self) -> HashMap<String, Vec<usize>> {
        let eng = match self.engine.lock() {
            Ok(e) => e,
            Err(_) => return HashMap::new(),
        };
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

    /// Reconcile input streams with the engine's armed tracks.
    /// Only call when the engine guard is NOT held (startup, arm/input
    /// buttons after drop). do_play/do_rec call ensure_inputs_for
    /// instead: they run under a held guard, and re-locking the same
    /// non-reentrant Mutex deadlocked the UI thread on 2026-10-26
    /// (window froze on PLAY after REC, GNOME force-quit it).
    pub fn ensure_inputs(&mut self) {
        let wanted = self.wanted_inputs();
        self.ensure_inputs_for(&wanted);
    }

    /// Reconcile input streams with a caller-supplied wanted map.
    /// Never locks the engine, so it is safe to call while the caller
    /// already holds the engine guard.
    pub fn ensure_inputs_for(&mut self, wanted: &HashMap<String, Vec<usize>>) {
        // Stop streams nobody wants anymore.
        let current: Vec<String> = self.input_streams.keys().cloned().collect();
        for name in current {
            if !wanted.contains_key(&name) {
                self.input_streams.remove(&name);
                if let Ok(mut subs) = self.input_subs.lock() {
                    subs.remove(&name);
                }
            }
        }
        // Start streams for newly wanted devices.
        for (name, tracks) in wanted.iter() {
            if self.input_streams.contains_key(name) {
                if let Ok(mut subs) = self.input_subs.lock() {
                    subs.insert(name.clone(), tracks.clone());
                }
                continue;
            }
            let device = match Self::find_input_device(name) {
                Some(d) => d,
                None => {
                    self.last_error = Some(format!("input device gone: {name}"));
                    continue;
                }
            };
            // Prefer mono; fall back to channel 0 of a wider config.
            let channels: u16 = match device.default_input_config() {
                Ok(c) => c.channels(),
                Err(_) => 1,
            };
            let config = StreamConfig {
                channels,
                sample_rate: SampleRate(self.sample_rate),
                buffer_size: cpal::BufferSize::Default,
            };
            let engine = self.engine.clone();
            let hub = self.hub.clone();
            let subs_map = self.input_subs.clone();
            let dev_name = name.clone();
            let stream = device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let ch = channels as usize;
                    if ch == 0 || data.is_empty() {
                        return;
                    }
                    let mono: Vec<f32> = if ch == 1 {
                        data.to_vec()
                    } else {
                        data.iter().step_by(ch).copied().collect()
                    };
                    let tracks: Vec<usize> = subs_map
                        .lock()
                        .map(|m| m.get(&dev_name).cloned().unwrap_or_default())
                        .unwrap_or_default();
                    if tracks.is_empty() {
                        return;
                    }
                    if let Ok(mut eng) = engine.lock() {
                        for &t in &tracks {
                            eng.push_input(t, &mono);
                        }
                    }
                    for &t in &tracks {
                        hub.push(t, &mono);
                    }
                },
                move |err| eprintln!("console-8-rs input error: {err}"),
                None,
            );
            match stream {
                Ok(s) => {
                    if s.play().is_ok() {
                        self.input_streams.insert(name.clone(), s);
                        if let Ok(mut subs) = self.input_subs.lock() {
                            subs.insert(name.clone(), tracks.clone());
                        }
                        self.last_error = None;
                    } else {
                        self.last_error =
                            Some(format!("could not start input: {name}"));
                    }
                }
                Err(e) => {
                    self.last_error = Some(format!("input {name} failed ({e})"));
                }
            }
        }
    }

    /// Armed PipeWire node names, read under the engine lock.
    fn wanted_pw_inputs(&self) -> HashMap<String, Vec<usize>> {
        let eng = match self.engine.lock() {
            Ok(e) => e,
            Err(_) => return HashMap::new(),
        };
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

    /// Reconcile PipeWire captures with the engine's armed tracks.
    /// Only call when the engine guard is NOT held (same rule as
    /// ensure_inputs); do_play/do_rec use ensure_pw_inputs_for.
    pub fn ensure_pw_inputs(&mut self) {
        let wanted = self.wanted_pw_inputs();
        self.ensure_pw_inputs_for(&wanted);
    }

    /// Reconcile PipeWire captures with a caller-supplied wanted map.
    /// Never locks the engine and never enumerates: capture threads
    /// resolve node names to ids themselves, so this returns
    /// immediately even if the daemon is slow. Stopping a capture only
    /// sends a quit message (never joins), so it is safe under the
    /// engine guard.
    pub fn ensure_pw_inputs_for(&mut self, wanted: &HashMap<String, Vec<usize>>) {
        // Stop captures nobody wants anymore (drop asks the thread to
        // quit; the thread exits on its own).
        let current: Vec<String> = self.pw_streams.keys().cloned().collect();
        for name in current {
            if !wanted.contains_key(&name) {
                self.pw_streams.remove(&name);
                if let Ok(mut subs) = self.pw_subs.lock() {
                    subs.remove(&name);
                }
            }
        }
        // Start captures for newly wanted nodes.
        for (name, tracks) in wanted.iter() {
            if self.pw_streams.contains_key(name) {
                if let Ok(mut subs) = self.pw_subs.lock() {
                    subs.insert(name.clone(), tracks.clone());
                }
                continue;
            }
            if let Ok(mut subs) = self.pw_subs.lock() {
                subs.insert(name.clone(), tracks.clone());
            }
            match PwCapture::start(
                self.engine.clone(),
                self.hub.clone(),
                self.pw_subs.clone(),
                name,
                self.sample_rate,
                self.pw_error.clone(),
            ) {
                Ok(cap) => {
                    self.pw_streams.insert(name.clone(), cap);
                    self.last_error = None;
                }
                Err(e) => {
                    self.last_error = Some(e);
                }
            }
        }
    }

    /// Feed a chunk from an external source (used by tests / future
    /// bridges). Normal device input arrives via the cpal callback.
    pub fn push_input(&self, track: usize, frames: &[f32]) {
        if let Ok(mut eng) = self.engine.lock() {
            eng.push_input(track, frames);
        }
        self.hub.push(track, frames);
    }
}
