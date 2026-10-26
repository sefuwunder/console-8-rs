//! Disk-based WAV I/O for console-8-rs.
//! Takes are plain mono 32-bit float WAV files: the recorder streams
//! frames to disk as they arrive (so a take can run as long as there is
//! disk space), and the playback engine seeks and reads them back.
//! The writer patches the RIFF sizes on finalize, and best-effort on
//! drop, so even a crashed take stays a readable file.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const HEADER_LEN: u64 = 44;

fn write_u32_le(w: &mut impl Write, v: u32) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_u16_le(w: &mut impl Write, v: u16) -> std::io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn write_tag(w: &mut impl Write, tag: &[u8; 4]) -> std::io::Result<()> {
    w.write_all(tag)
}

/// Streaming WAV writer. Created with placeholder sizes, finalized on
/// `finalize()` (or best-effort on drop).
pub struct WavWriter {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    sample_rate: u32,
    frames: u64,
    finalized: bool,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        write_tag(&mut file, b"RIFF")?;
        write_u32_le(&mut file, 0)?; // patched on finalize
        write_tag(&mut file, b"WAVE")?;
        write_tag(&mut file, b"fmt ")?;
        write_u32_le(&mut file, 16)?;
        write_u16_le(&mut file, 3)?; // IEEE float
        write_u16_le(&mut file, 1)?; // mono
        write_u32_le(&mut file, sample_rate)?;
        write_u32_le(&mut file, sample_rate * 4)?;
        write_u16_le(&mut file, 4)?; // block align
        write_u16_le(&mut file, 32)?; // bits per sample
        write_tag(&mut file, b"data")?;
        write_u32_le(&mut file, 0)?; // patched on finalize
        file.flush()?;
        Ok(WavWriter {
            path: path.to_path_buf(),
            file: Some(file),
            sample_rate,
            frames: 0,
            finalized: false,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn frames_written(&self) -> u64 {
        self.frames
    }

    pub fn write(&mut self, frames: &[f32]) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            let mut buf = Vec::with_capacity(frames.len() * 4);
            for &x in frames {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            f.write_all(&buf)?;
            self.frames += frames.len() as u64;
        }
        Ok(())
    }

    fn patch_sizes(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.flush()?;
            let data_bytes = (self.frames * 4) as u32;
            f.seek(SeekFrom::Start(4))?;
            write_u32_le(f, 36u32.wrapping_add(data_bytes))?;
            f.seek(SeekFrom::Start(40))?;
            write_u32_le(f, data_bytes)?;
            f.flush()?;
        }
        Ok(())
    }

    pub fn finalize(mut self) -> std::io::Result<u64> {
        self.patch_sizes()?;
        self.finalized = true;
        Ok(self.frames)
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = self.patch_sizes();
        }
    }
}

/// Seekable WAV reader: supports IEEE float (tag 3) and 16-bit PCM
/// (tag 1), mono or multi-channel (channel 0 is read for playback).
pub struct WavReader {
    file: BufReader<File>,
    sample_rate: u32,
    channels: u16,
    format: u16,
    data_start: u64,
    total_frames: u64,
    pos: u64,
}

impl WavReader {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut raw = File::open(path)?;
        let mut hdr = [0u8; 12];
        raw.read_exact(&mut hdr)?;
        if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a RIFF/WAVE file",
            ));
        }
        let mut fmt: Option<(u16, u16, u32)> = None; // format, channels, rate
        let mut data: Option<(u64, u64)> = None; // start, byte len
        loop {
            let mut chunk_hdr = [0u8; 8];
            if raw.read_exact(&mut chunk_hdr).is_err() {
                break;
            }
            let tag = &chunk_hdr[0..4];
            let size = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap()) as u64;
            let chunk_start = raw.stream_position()?;
            match tag {
                b"fmt " => {
                    let mut f = vec![0u8; size as usize];
                    raw.read_exact(&mut f)?;
                    let format = u16::from_le_bytes(f[0..2].try_into().unwrap());
                    let channels = u16::from_le_bytes(f[2..4].try_into().unwrap());
                    let rate = u32::from_le_bytes(f[4..8].try_into().unwrap());
                    fmt = Some((format, channels, rate));
                }
                b"data" => {
                    data = Some((chunk_start, size));
                    break;
                }
                _ => {
                    raw.seek(SeekFrom::Current(size as i64))?;
                }
            }
            // Chunks are word-aligned.
            if size % 2 == 1 {
                raw.seek(SeekFrom::Current(1))?;
            }
        }
        let (format, channels, rate) = fmt.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAV has no fmt chunk")
        })?;
        let (data_start, data_len) = data.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAV has no data chunk")
        })?;
        if format != 1 && format != 3 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported WAV format tag {format}"),
            ));
        }
        let bytes_per_frame = channels as u64 * if format == 3 { 4 } else { 2 };
        let total_frames = data_len / bytes_per_frame;
        let mut file = BufReader::with_capacity(65536, raw);
        file.seek(SeekFrom::Start(data_start))?;
        Ok(WavReader {
            file,
            sample_rate: rate,
            channels,
            format,
            data_start,
            total_frames,
            pos: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn seek_frame(&mut self, frame: u64) -> std::io::Result<()> {
        let frame = frame.min(self.total_frames);
        let bytes_per_frame = self.channels as u64 * if self.format == 3 { 4 } else { 2 };
        self.file
            .seek(SeekFrom::Start(self.data_start + frame * bytes_per_frame))?;
        self.pos = frame;
        Ok(())
    }

    /// Fill `out` with channel 0, starting at the current position.
    /// Returns frames actually read (0 at EOF).
    pub fn read_frames(&mut self, out: &mut [f32]) -> std::io::Result<usize> {
        let want = (out.len() as u64).min(self.total_frames.saturating_sub(self.pos)) as usize;
        if want == 0 {
            return Ok(0);
        }
        let bytes_per_frame = self.channels as usize * if self.format == 3 { 4 } else { 2 };
        let mut raw = vec![0u8; want * bytes_per_frame];
        self.file.read_exact(&mut raw)?;
        for i in 0..want {
            let off = i * bytes_per_frame;
            out[i] = if self.format == 3 {
                f32::from_le_bytes(raw[off..off + 4].try_into().unwrap())
            } else {
                i16::from_le_bytes(raw[off..off + 2].try_into().unwrap()) as f32 / 32768.0
            };
        }
        self.pos += want as u64;
        Ok(want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("console8rs-test-{name}.wav"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn round_trip_float_wav() {
        let path = tmp_path("roundtrip");
        let sr = 48000;
        let mut w = WavWriter::create(&path, sr).unwrap();
        let frames: Vec<f32> = (0..4800).map(|i| (i as f32 / 4800.0) * 2.0 - 1.0).collect();
        w.write(&frames[..2400]).unwrap();
        w.write(&frames[2400..]).unwrap();
        assert_eq!(w.frames_written(), 4800);
        w.finalize().unwrap();
        let mut r = WavReader::open(&path).unwrap();
        assert_eq!(r.sample_rate(), sr);
        assert_eq!(r.total_frames(), 4800);
        let mut back = vec![0.0; 4800];
        assert_eq!(r.read_frames(&mut back).unwrap(), 4800);
        for (a, b) in frames.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
        // Seek + partial read.
        r.seek_frame(100).unwrap();
        let mut part = vec![0.0; 10];
        assert_eq!(r.read_frames(&mut part).unwrap(), 10);
        assert!((part[0] - frames[100]).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropped_writer_still_readable() {
        let path = tmp_path("drop");
        {
            let mut w = WavWriter::create(&path, 48000).unwrap();
            w.write(&[0.25, -0.25, 0.5]).unwrap();
            // no finalize: drop patches sizes
        }
        let mut r = WavReader::open(&path).unwrap();
        assert_eq!(r.total_frames(), 3);
        let mut back = vec![0.0; 3];
        assert_eq!(r.read_frames(&mut back).unwrap(), 3);
        assert!((back[0] - 0.25).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reader_reads_16bit_pcm() {
        let path = tmp_path("pcm16");
        {
            let mut f = BufWriter::new(File::create(&path).unwrap());
            write_tag(&mut f, b"RIFF").unwrap();
            write_u32_le(&mut f, 36 + 8).unwrap();
            write_tag(&mut f, b"WAVE").unwrap();
            write_tag(&mut f, b"fmt ").unwrap();
            write_u32_le(&mut f, 16).unwrap();
            write_u16_le(&mut f, 1).unwrap();
            write_u16_le(&mut f, 1).unwrap();
            write_u32_le(&mut f, 48000).unwrap();
            write_u32_le(&mut f, 96000).unwrap();
            write_u16_le(&mut f, 2).unwrap();
            write_u16_le(&mut f, 16).unwrap();
            write_tag(&mut f, b"data").unwrap();
            write_u32_le(&mut f, 8).unwrap();
            write_u16_le(&mut f, 16384).unwrap();
            write_u16_le(&mut f, 0).unwrap();
            write_u16_le(&mut f, (-16384i16) as u16).unwrap();
            write_u16_le(&mut f, 32767).unwrap();
            f.flush().unwrap();
        }
        let mut r = WavReader::open(&path).unwrap();
        assert_eq!(r.total_frames(), 4);
        let mut back = vec![0.0; 4];
        assert_eq!(r.read_frames(&mut back).unwrap(), 4);
        assert!((back[0] - 0.5).abs() < 1e-4);
        assert!((back[2] + 0.5).abs() < 1e-4);
        let _ = std::fs::remove_file(&path);
    }
}
