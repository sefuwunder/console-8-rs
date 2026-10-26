//! PipeWire source enumeration and capture.
//!
//! ALSA enumeration cannot see PipeWire monitor nodes, so recording
//! internal app audio (a drum machine, a synth, anything playing through
//! a PipeWire sink) needs a native PipeWire input. Each capture runs on
//! its own thread with its own MainLoop; the thread resolves the node id
//! for a stored node name itself, so reconciling inputs never blocks the
//! UI thread on enumeration. Stopping sends a quit message over a
//! pipewire channel and detaches: joining the capture thread from the UI
//! thread while it holds the engine guard would deadlock exactly like
//! the 2026-10-26 PLAY-after-REC freeze, so stop() never blocks.
//!
//! Build requirement: libpipewire-0.3-dev (the daemon itself only needs
//! to be running at capture time, not at build time).

use crate::audio::MonitorHub;
use crate::engine::Engine;
use pipewire as pw;
use pw::properties::properties;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamState};
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn init_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(pw::init);
}

/// One PipeWire recordable node: an Audio/Source (a mic) or an
/// Audio/Sink (captured via its monitor ports: internal app audio).
/// `name` is the stable node name used for capture; `description` is
/// the human-readable label shown by --list-devices. `is_sink` marks
/// sink monitors, listed after sources so the input cycle stays
/// predictable.
///
/// Correction (2026-10-26, lab-verified against a live daemon): sink
/// monitors are NOT Audio/Source nodes. A sink exposes monitor_FL /
/// monitor_FR output ports on the sink node itself; no separate
/// ".monitor" node exists in native PipeWire (that naming is
/// pipewire-pulse's). Filtering on Audio/Source alone could therefore
/// never list a monitor, which is why only the mic ever appeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PwSource {
    pub name: String,
    pub description: String,
    pub is_sink: bool,
}

/// Enumerate recordable nodes (Audio/Source + Audio/Sink), with a
/// timeout so a wedged daemon can never freeze the UI thread. The
/// roundtrip itself runs on a worker thread; on timeout the thread is
/// abandoned (it exits on its own once the daemon answers).
pub fn list_sources_timeout(timeout: Duration) -> Result<Vec<PwSource>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(list_sources_sync());
    });
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err("pipewire enumeration timed out".to_string()),
    }
}

fn list_sources_sync() -> Result<Vec<PwSource>, String> {
    init_once();
    let mainloop = pw::main_loop::MainLoop::new(None).map_err(|e| format!("pipewire: {e}"))?;
    let context =
        pw::context::Context::new(&mainloop).map_err(|e| format!("pipewire: {e}"))?;
    let core = context.connect(None).map_err(|e| format!("pipewire: {e}"))?;
    let nodes = collect_nodes_on(&mainloop, &core)?;
    let mut out: Vec<PwSource> = nodes
        .into_iter()
        .map(|(_, name, desc, is_sink)| PwSource {
            name,
            description: desc,
            is_sink,
        })
        .collect();
    out.sort_by(|a, b| (a.is_sink, &a.description).cmp(&(b.is_sink, &b.description)));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// One registry roundtrip: (id, node.name, node.description, is_sink)
/// for every Audio/Source and Audio/Sink node. Runs on the calling
/// thread's MainLoop.

/// Little-endian bytes -> f32 samples (truncates a trailing partial
/// sample). Pure, so it is unit-testable without a daemon.
pub fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Interleaved samples -> mono by averaging channels. A monitor tap is
/// stereo program material, so averaging keeps both sides (unlike the
/// ALSA path, which takes channel 0).
pub fn to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum = 0.0f32;
        for c in 0..channels {
            sum += samples[f * channels + c];
        }
        out.push(sum / channels as f32);
    }
    out
}

/// The EnumFormat pod offered to the graph: f32, engine rate, stereo
/// (downmixed to mono in the process callback). PipeWire resamples the
/// monitor to the requested rate, so the take header rate stays true.
pub fn format_param_bytes(sample_rate: u32, channels: u32) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(sample_rate);
    info.set_channels(channels);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .unwrap()
        .0
        .into_inner()
}

struct StopMsg;

struct CaptureData {
    engine: Arc<Mutex<Engine>>,
    hub: Arc<MonitorHub>,
    subs: Arc<Mutex<HashMap<String, Vec<usize>>>>,
    node_name: String,
    format: AudioInfoRaw,
    err: Arc<Mutex<Option<String>>>,
}

/// A running PipeWire capture. Dropping it asks the capture thread to
/// quit; the thread owns its MainLoop/Context/Core/Stream and exits on
/// its own, so drop never blocks the UI thread.
pub struct PwCapture {
    sender: pw::channel::Sender<StopMsg>,
}

impl PwCapture {
    pub fn start(
        engine: Arc<Mutex<Engine>>,
        hub: Arc<MonitorHub>,
        subs: Arc<Mutex<HashMap<String, Vec<usize>>>>,
        node_name: &str,
        sample_rate: u32,
        err: Arc<Mutex<Option<String>>>,
    ) -> Result<PwCapture, String> {
        init_once();
        let (tx, rx) = pw::channel::channel::<StopMsg>();
        let name = node_name.to_string();
        std::thread::spawn(move || {
            capture_thread(engine, hub, subs, name, sample_rate, err, rx);
        });
        Ok(PwCapture { sender: tx })
    }

    /// Ask the capture thread to quit. Never blocks: the thread may be
    /// inside its process callback waiting on the engine lock, and the
    /// caller may hold that lock (do_play/do_rec run under the guard).
    pub fn stop(&self) {
        let _ = self.sender.send(StopMsg);
    }
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_thread(
    engine: Arc<Mutex<Engine>>,
    hub: Arc<MonitorHub>,
    subs: Arc<Mutex<HashMap<String, Vec<usize>>>>,
    node_name: String,
    sample_rate: u32,
    err: Arc<Mutex<Option<String>>>,
    stop_rx: pw::channel::Receiver<StopMsg>,
) {
    let set_err = |m: String| {
        if let Ok(mut e) = err.lock() {
            *e = Some(m);
        }
    };
    let mainloop = match pw::main_loop::MainLoop::new(None) {
        Ok(m) => m,
        Err(e) => {
            set_err(format!("pipewire: {e}"));
            return;
        }
    };
    // Quit on stop message, from any thread.
    let _stop_attach = stop_rx.attach(mainloop.loop_(), {
        let ml = mainloop.clone();
        move |_| ml.quit()
    });
    let context = match pw::context::Context::new(&mainloop) {
        Ok(c) => c,
        Err(e) => {
            set_err(format!("pipewire: {e}"));
            return;
        }
    };
    let core = match context.connect(None) {
        Ok(c) => c,
        Err(e) => {
            set_err(format!("pipewire: {e}"));
            return;
        }
    };
    // Resolve the stored node name to a live id (ids are transient;
    // names are stable). Fall back to the description for nodes whose
    // driver renamed them.
    let nodes = match collect_nodes_on(&mainloop, &core) {
        Ok(n) => n,
        Err(e) => {
            set_err(e);
            return;
        }
    };
    let node_id = nodes
        .iter()
        .find(|(_, n, _, _)| n == &node_name)
        .or_else(|| nodes.iter().find(|(_, _, d, _)| d == &node_name))
        .map(|(id, _, _, _)| *id);
    let Some(node_id) = node_id else {
        set_err(format!("pipewire source gone: {node_name}"));
        return;
    };
    let stream = match pw::stream::Stream::new(
        &core,
        "console-8-rs-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            set_err(format!("pipewire: {e}"));
            return;
        }
    };
    let data = CaptureData {
        engine,
        hub,
        subs,
        node_name,
        format: AudioInfoRaw::new(),
        err: err.clone(),
    };
    let _listener = match stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((mt, st)) = pw::spa::param::format_utils::parse_format(param) else {
                return;
            };
            if mt != pw::spa::param::format::MediaType::Audio
                || st != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let _ = data.format.parse(param);
        })
        .state_changed(|_, data, _old, new| {
            if let StreamState::Error(msg) = new {
                if let Ok(mut e) = data.err.lock() {
                    *e = Some(format!("pipewire stream error: {msg}"));
                }
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let d = &mut datas[0];
            let channels = data.format.channels() as usize;
            if data.format.format() != AudioFormat::F32LE || channels == 0 {
                return;
            }
            let size = d.chunk().size() as usize;
            let Some(bytes) = d.data() else { return };
            let bytes = &bytes[..bytes.len().min(size)];
            let mono = to_mono(&bytes_to_f32(bytes), channels);
            if mono.is_empty() {
                return;
            }
            let tracks = data
                .subs
                .lock()
                .map(|m| m.get(&data.node_name).cloned().unwrap_or_default())
                .unwrap_or_default();
            if tracks.is_empty() {
                return;
            }
            if let Ok(mut eng) = data.engine.lock() {
                for &t in &tracks {
                    eng.push_input(t, &mono);
                }
            }
            for &t in &tracks {
                data.hub.push(t, &mono);
            }
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => {
            set_err(format!("pipewire: {e}"));
            return;
        }
    };
    let param_bytes = format_param_bytes(sample_rate, 2);
    let mut params = [Pod::from_bytes(&param_bytes).unwrap()];
    if let Err(e) = stream.connect(
        Direction::Input,
        Some(node_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        set_err(format!("pipewire connect failed: {e}"));
        return;
    }
    mainloop.run();
}

/// Registry roundtrip on an already-running MainLoop/Context pair
/// (capture threads already own one; a fresh enumeration builds one in
/// list_sources_sync).
fn collect_nodes_on(
    mainloop: &pw::main_loop::MainLoop,
    core: &pw::core::Core,
) -> Result<Vec<(u32, String, String, bool)>, String> {
    let registry = core.get_registry().map_err(|e| format!("pipewire: {e}"))?;
    let nodes: Rc<RefCell<Vec<(u32, String, String, bool)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let nodes2 = nodes.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = obj.props else { return };
            // Sources AND sinks: a sink's monitor is its own output
            // ports, not a separate node (see PwSource docs).
            let is_sink = match props.get("media.class") {
                Some("Audio/Source") => false,
                Some("Audio/Sink") => true,
                _ => return,
            };
            let Some(name) = props.get("node.name") else {
                return;
            };
            let desc = props.get("node.description").unwrap_or(name).to_string();
            nodes2
                .borrow_mut()
                .push((obj.id, name.to_string(), desc, is_sink));
        })
        .register();
    let done = Rc::new(Cell::new(false));
    let done2 = done.clone();
    let loop2 = mainloop.clone();
    let pending = core.sync(0).map_err(|e| format!("pipewire: {e}"))?;
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done2.set(true);
                loop2.quit();
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
    let result: Vec<(u32, String, String, bool)> = nodes.borrow().iter().cloned().collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_to_f32_round_trips() {
        let vals = [0.5f32, -0.25, 0.0, 1.0];
        let mut bytes = Vec::new();
        for v in vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(bytes_to_f32(&bytes), vals);
        // Trailing partial sample is dropped, not panicked on.
        bytes.push(0x00);
        assert_eq!(bytes_to_f32(&bytes), vals);
    }

    #[test]
    fn to_mono_averages_channels() {
        assert_eq!(to_mono(&[0.5, -0.5, 0.25, 0.25], 2), vec![0.0, 0.25]);
        assert_eq!(to_mono(&[0.5, -0.5], 1), vec![0.5, -0.5]);
        assert_eq!(to_mono(&[0.5, -0.5, 0.25], 2), vec![0.0]);
    }

    #[test]
    fn format_param_round_trips() {
        let bytes = format_param_bytes(48000, 2);
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut info = AudioInfoRaw::new();
        info.parse(&pod).unwrap();
        assert_eq!(info.format(), AudioFormat::F32LE);
        assert_eq!(info.rate(), 48000);
        assert_eq!(info.channels(), 2);
    }

    /// Manual live test: needs a PipeWire daemon plus audio playing
    /// into a sink (e.g., a null sink fed by pw-play). NOT run by the
    /// default suite (no daemon in CI/VM). Run with:
    ///   cargo test --release capture_from_live_sink_monitor -- --ignored --nocapture
    /// Env PW_TEST_NODE overrides which node name to capture;
    /// otherwise the first Audio/Sink wins. The sink's monitor ports
    /// carry the sink's mix; the capture stream connects to the sink
    /// node id directly, exactly like a source.
    #[test]
    #[ignore]
    fn capture_from_live_sink_monitor() {
        let dir = std::env::temp_dir().join("console8rs-pw-live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("takes")).unwrap();
        let engine = Arc::new(Mutex::new(Engine::new(
            crate::session::Session::new("t", 48000),
            dir.clone(),
        )));
        let hub = Arc::new(MonitorHub::new());
        let subs = Arc::new(Mutex::new(HashMap::new()));
        let err = Arc::new(Mutex::new(None::<String>));
        let nodes = list_sources_timeout(Duration::from_secs(5))
            .expect("daemon must be reachable for the live test");
        let wanted = std::env::var("PW_TEST_NODE").ok();
        let node = wanted
            .as_deref()
            .and_then(|w| nodes.iter().find(|s| s.name == w))
            .or_else(|| nodes.iter().find(|s| s.is_sink))
            .expect("no sink found; play audio into a sink first");
        eprintln!("capturing monitor of {} ({})", node.description, node.name);
        subs.lock()
            .unwrap()
            .insert(node.name.clone(), vec![0]);
        let cap = PwCapture::start(
            engine.clone(),
            hub.clone(),
            subs.clone(),
            &node.name,
            48000,
            err.clone(),
        )
        .expect("capture thread must start");
        // Playback must already be running externally; collect a few
        // seconds (PW_TEST_SLEEP_SECS stretches the window for graph
        // inspection during a live run).
        let sleep_secs: u64 = std::env::var("PW_TEST_SLEEP_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        std::thread::sleep(Duration::from_secs(sleep_secs));
        cap.stop();
        let frames = hub.drain(0, 48000 * 3);
        let peak = frames.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        eprintln!("captured {} frames, peak {}", frames.len(), peak);
        assert!(
            err.lock().unwrap().is_none(),
            "capture error: {:?}",
            err.lock().unwrap()
        );
        assert!(
            peak > 0.05,
            "sink-monitor capture must carry audible audio, got peak {peak}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
