# console-8-rs

An 8-track tape recorder and mixer in Rust + SDL2, built as the Rust
sibling of `console-8` (the Bun/WebAudio one). The difference is the
brief: real disk streaming, real input selection, and a real master
bus — not a browser demo.

## What it is

- **8 tracks of tape.** Each take is a mono 32-bit float WAV file
  under `sessions/<name>/takes/`, streamed to disk as it is recorded
  and streamed back for playback. A take runs as long as there is
  disk space; nothing but one chunk queue sits in RAM.
- **Per-track input selection.** Click a track's input button to
  cycle: `NO INPUT` → `TONE` → every input device cpal enumerates →
  back. A mic is just a device. App audio is a device too, if the
  audio server exposes it:
  - **PipeWire / PulseAudio:** monitor sources (e.g. "Monitor of
    Built-in Audio") appear as input devices; pick one and that
    track records whatever the apps are playing.
  - **Plain ALSA:** load `snd-aloop` and route apps to the loopback;
    it appears as a device.
  - **No hardware / testing:** `TONE` is a built-in 440 Hz source
    that records and monitors with no device at all.
  - `console-8-rs --list-devices` shows what your machine exposes.
- **Splice and arrange.** The timeline is the tape: click a region
  to select it, drag it to move it, drag its edges to trim it,
  `S` (or the SPLIT button) splices the selected region at the
  playhead, `DEL` deletes it. Regions can move across tracks.
- **SSL-style channel strips that actually process audio.** Per
  track: trim, polarity, high-pass, 4-band EQ (LF shelf, two
  sweepable parametric mids, HF shelf), compressor with
  auto-makeup, pan, fader, mute/solo, peak meter. Drag knobs
  vertically; the mouse wheel nudges the knob under the cursor.
- **Master bus.** Tube saturation is **always on** and measures
  ~1.5% THD on a -6 dBFS 1 kHz sine (the test asserts 1.0–2.0%;
  distortion rises with level, as on hardware). Then a maximiser
  knob (dry/saturated blend), the master fader, and a linked
  brickwall limiter at -0.5 dBFS that never overshoots.
- **Sessions are plain text.** `sessions/<name>/session.txt` holds
  regions, strip settings, inputs, and the playhead; takes are
  referenced by file name. The session saves on STOP and on SAVE,
  and loads on start, so a take survives a restart.

## Workflow

1. Click a track's input button until it shows your mic (or a
   monitor source for app audio, or TONE to test).
2. Click ARM on that track. The input now monitors through its
   strip, even with the transport stopped.
3. Press REC (arms record mode), then PLAY. The armed tracks cut
   takes from the playhead while the existing tape plays back.
   Press REC again while playing to punch out; STOP ends the
   take and saves the session.
4. Arrange on the timeline: split, move, trim, delete.
5. Mix with the strips and the master bus. The meters show
   post-fader track level and master L/R.

Keys: `Space` play/stop, `R` record mode / punch in-out, `S`
split selected at playhead, `Del` delete selected, `Esc` clear
selection, `Left`/`Right` playhead ±1 s (`Shift` ±10 s),
`Ctrl+S` save.

## Build and run

```sh
# Needs libsdl2-dev, libasound2-dev, libpipewire-0.3-dev and
# libclang-dev (Debian/Ubuntu; libclang is for the PipeWire bindings'
# bindgen step, needed at build time only).
cargo build --release
./target/release/console-8-rs [--session <dir>]

# Headless checks (no audio or display hardware needed):
./target/release/console-8-rs --list-devices
./target/release/console-8-rs --smoke 3
./target/release/console-8-rs --render /tmp/mix.wav 5
./target/release/console-8-rs --shot /tmp/faceplate.bmp
```

Run from the repo root so `sessions/` lands next to the binary;
`sessions/` is gitignored — takes are your audio, never pushed.

## Inputs: mic and internal app audio

Each track's input button cycles `NO INPUT` → `TONE` → ALSA devices
→ PipeWire sources. PipeWire sources include sink monitors, which
is how internal app audio is captured: start the drum machine or
synth so its sink exists, arm a track, cycle its input to the
`...monitor` node, then REC + PLAY. The monitor is downmixed to
mono and recorded raw, pre-strip, like any other input.
`--list-devices` prints PipeWire sources with descriptions so you
can identify the right monitor. PipeWire capture runs on its own
thread per node; if the daemon is absent or a node vanishes, the
footer shows the error instead of freezing.

## Signal chain

```
take file -> region window -> track strip
  (trim -> polarity -> HPF -> LF/LMF/HMF/HF -> compressor)
  -> pan -> fader -> mute/solo
    -> sum -> tube (1.5%) -> maximiser -> master fader
      -> brickwall limiter (-0.5 dBFS) -> output
```

Recorded audio is the raw input, pre-strip: the strip shapes the
monitor and the mix, never the take itself.

## Honest limits

- Live capture is verified on real hardware for the mic only; this
  VM has no sound devices and no PipeWire daemon, so PipeWire
  enumeration and capture are verified structurally (format and
  conversion unit tests, a no-daemon failure test) and the
  record/mix paths end to end from files. Try a monitor node on
  your machine and report what `--list-devices` shows.
- The engine shares one Mutex between the UI and the audio
  callbacks; a disk stall in the render callback can briefly stall
  input. Fine for an 8-track; a lock-free design is the next step
  if dropouts ever show up.
- No punch-in at an arbitrary point mid-take beyond REC-while-
  playing, no crossfades at region joins, no loop playback, no
  sample-rate conversion (a session plays at its own rate; the
  output stream is requested at that rate and fails visibly if the
  device refuses it).
- 21/21 tests green. `--smoke` renders a synthetic mix and checks
  it is finite, audible, and under the limiter ceiling.
