//! DSP for console-8-rs: SSL-style channel strips and the master bus.
//! Everything is plain f32 math, no dependencies, so the whole chain is
//! unit-testable without audio hardware.
//!
//! Channel strip order: trim -> polarity -> 4-band EQ + high-pass ->
//! compressor (auto-makeup) -> pan / fader (applied by the engine).
//! Master bus order: tube (always on, ~1.5% THD at -6 dBFS) ->
//! maximiser -> master fader (engine) -> brickwall limiter.

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn linear_to_db(x: f32) -> f32 {
    if x <= 0.0 {
        -120.0
    } else {
        20.0 * x.log10()
    }
}

/// RBJ cookbook biquad, Direct Form I.
#[derive(Clone, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new(b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        Biquad {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn from_rbj(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Biquad::new(b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    fn coeffs(freq: f32, q: f32, sr: f32) -> (f32, f32, f32) {
        let w = 2.0 * std::f32::consts::PI * (freq / sr).min(0.45);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        (sin, cos, alpha)
    }

    pub fn lowpass(freq: f32, q: f32, sr: f32) -> Self {
        let (_, cos, alpha) = Self::coeffs(freq, q, sr);
        let b0 = (1.0 - cos) / 2.0;
        let b1 = 1.0 - cos;
        let b2 = (1.0 - cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self::from_rbj(b0, b1, b2, a0, a1, a2)
    }

    pub fn highpass(freq: f32, q: f32, sr: f32) -> Self {
        let (_, cos, alpha) = Self::coeffs(freq, q, sr);
        let b0 = (1.0 + cos) / 2.0;
        let b1 = -(1.0 + cos);
        let b2 = (1.0 + cos) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;
        Self::from_rbj(b0, b1, b2, a0, a1, a2)
    }

    pub fn peaking(freq: f32, q: f32, gain_db: f32, sr: f32) -> Self {
        let (_, cos, alpha) = Self::coeffs(freq, q, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha / a;
        Self::from_rbj(b0, b1, b2, a0, a1, a2)
    }

    pub fn low_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let (_, cos, alpha) = Self::coeffs(freq, 0.7071, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let sq = 2.0 * alpha * a.sqrt();
        let b0 = a * ((a + 1.0) - (a - 1.0) * cos + sq);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos - sq);
        let a0 = (a + 1.0) + (a - 1.0) * cos + sq;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos);
        let a2 = (a + 1.0) + (a - 1.0) * cos - sq;
        Self::from_rbj(b0, b1, b2, a0, a1, a2)
    }

    pub fn high_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let (_, cos, alpha) = Self::coeffs(freq, 0.7071, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let sq = 2.0 * alpha * a.sqrt();
        let b0 = a * ((a + 1.0) + (a - 1.0) * cos + sq);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos - sq);
        let a0 = (a + 1.0) - (a - 1.0) * cos + sq;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
        let a2 = (a + 1.0) - (a - 1.0) * cos - sq;
        Self::from_rbj(b0, b1, b2, a0, a1, a2)
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Replace coefficients without touching filter state, so live
    /// parameter changes don't click.
    pub fn set_from(&mut self, other: &Biquad) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// SSL-style 4-band EQ: LF shelf, two parametric mids, HF shelf, plus a
/// high-pass filter. Frequencies for the shelves are fixed (120 Hz /
/// 8 kHz); the mids are sweepable.
#[derive(Clone, Debug)]
pub struct Eq {
    sr: f32,
    hpf: Biquad,
    lf: Biquad,
    lmf: Biquad,
    hmf: Biquad,
    hf: Biquad,
}

impl Eq {
    pub fn new(sr: f32) -> Self {
        Eq {
            sr,
            hpf: Biquad::highpass(20.0, 0.7071, sr),
            lf: Biquad::low_shelf(120.0, 0.0, sr),
            lmf: Biquad::peaking(800.0, 1.0, 0.0, sr),
            hmf: Biquad::peaking(3000.0, 1.0, 0.0, sr),
            hf: Biquad::high_shelf(8000.0, 0.0, sr),
        }
    }

    pub fn set(
        &mut self,
        hpf_freq: f32,
        lf_gain: f32,
        lmf_gain: f32,
        lmf_freq: f32,
        hmf_gain: f32,
        hmf_freq: f32,
        hf_gain: f32,
    ) {
        // set_from keeps filter state, so knob moves don't click.
        self.hpf
            .set_from(&Biquad::highpass(hpf_freq.max(20.0), 0.7071, self.sr));
        self.lf.set_from(&Biquad::low_shelf(120.0, lf_gain, self.sr));
        self.lmf
            .set_from(&Biquad::peaking(lmf_freq, 1.0, lmf_gain, self.sr));
        self.hmf
            .set_from(&Biquad::peaking(hmf_freq, 1.0, hmf_gain, self.sr));
        self.hf
            .set_from(&Biquad::high_shelf(8000.0, hf_gain, self.sr));
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.hpf.process(x);
        let y = self.lf.process(y);
        let y = self.lmf.process(y);
        let y = self.hmf.process(y);
        self.hf.process(y)
    }

    pub fn reset(&mut self) {
        self.hpf.reset();
        self.lf.reset();
        self.lmf.reset();
        self.hmf.reset();
        self.hf.reset();
    }
}

/// Feed-forward compressor with fixed 10 ms attack / 100 ms release and
/// auto-makeup: half the static gain reduction at threshold.
#[derive(Clone, Debug)]
pub struct Compressor {
    sr: f32,
    threshold_db: f32,
    ratio: f32,
    release_coeff: f32,
    env: f32,
    makeup: f32,
}

impl Compressor {
    pub fn new(sr: f32) -> Self {
        let mut c = Compressor {
            sr,
            threshold_db: -18.0,
            ratio: 4.0,
            release_coeff: 0.0,
            env: 0.0,
            makeup: 1.0,
        };
        c.set(-18.0, 4.0);
        c
    }

    pub fn set(&mut self, threshold_db: f32, ratio: f32) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.release_coeff = (-1.0 / (0.1 * self.sr)).exp();
        let depth = (-threshold_db).max(0.0);
        self.makeup = db_to_linear((depth * (1.0 - 1.0 / self.ratio) * 0.5).min(12.0));
    }

    pub fn gain_reduction_db(&self) -> f32 {
        let env_db = linear_to_db(self.env);
        if env_db <= self.threshold_db {
            0.0
        } else {
            (env_db - self.threshold_db) * (1.0 - 1.0 / self.ratio)
        }
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let a = x.abs();
        if a > self.env {
            self.env = a;
        } else {
            self.env *= self.release_coeff;
        }
        let gr = self.gain_reduction_db();
        x * db_to_linear(-gr) * self.makeup
    }

    pub fn reset(&mut self) {
        self.env = 0.0;
    }
}

/// Tube saturation, always on at the master bus. Gentle tanh drive plus
/// a small asymmetric term for the even harmonics tubes are known for.
/// TUBE_ASYM is tuned so a -6 dBFS 1 kHz sine measures ~1.5% THD
/// (see the tube_thd test); distortion rises with level, as on hardware.
pub const TUBE_DRIVE: f32 = 0.5;
pub const TUBE_ASYM: f32 = 0.056;

pub fn tube_drive(x: f32) -> f32 {
    (TUBE_DRIVE * (x + TUBE_ASYM * x * x)).tanh() / TUBE_DRIVE.tanh()
}

/// One-pole DC blocker (10 Hz-ish at 48 kHz) for after the tube stage.
#[derive(Clone, Debug, Default)]
pub struct DcBlocker {
    prev_in: f32,
    prev_out: f32,
}

impl DcBlocker {
    pub fn process(&mut self, x: f32) -> f32 {
        let y = x - self.prev_in + 0.995 * self.prev_out;
        self.prev_in = x;
        self.prev_out = y;
        y
    }
}

/// Maximiser: a blend of dry signal and normalized soft clipping.
/// amount 0 = unity, 1 = fully saturated. Output never exceeds 1.0 for
/// inputs within [-1, 1].
pub fn maximiser(x: f32, amount: f32) -> f32 {
    let m = amount.clamp(0.0, 1.0);
    if m <= 0.0 {
        return x;
    }
    let g = 1.0 + m * 7.0;
    (1.0 - m) * x + m * (g * x).tanh() / g.tanh()
}

/// Brickwall limiter: instant attack (the envelope never trails the
/// signal, so the gain is always sufficient), smoothed release.
/// Output never exceeds the ceiling.
#[derive(Clone, Debug)]
pub struct Limiter {
    ceiling: f32,
    release_coeff: f32,
    env: f32,
    gain: f32,
}

impl Limiter {
    pub fn new(sr: f32, ceiling_db: f32) -> Self {
        Limiter {
            ceiling: db_to_linear(ceiling_db),
            release_coeff: (-1.0 / (0.1 * sr)).exp(),
            env: 0.0,
            gain: 1.0,
        }
    }

    pub fn ceiling(&self) -> f32 {
        self.ceiling
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let (y, _) = self.process_stereo(x, 0.0);
        y
    }

    /// Linked stereo: one envelope from the louder channel, one gain
    /// for both, so the image doesn't shift on asymmetric peaks.
    pub fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        let a = l.abs().max(r.abs());
        if a > self.env {
            self.env = a;
        } else {
            self.env *= self.release_coeff;
        }
        let target = if self.env > 0.0 {
            (self.ceiling / self.env).min(1.0)
        } else {
            1.0
        };
        if target < self.gain {
            self.gain = target; // instant attack: never overshoots
        } else {
            self.gain += (target - self.gain) * (1.0 - self.release_coeff);
        }
        (l * self.gain, r * self.gain)
    }

    pub fn reset(&mut self) {
        self.env = 0.0;
        self.gain = 1.0;
    }
}

/// Peak meter with a ~300 ms fall.
#[derive(Clone, Debug)]
pub struct PeakMeter {
    decay: f32,
    peak: f32,
}

impl PeakMeter {
    pub fn new(sr: f32) -> Self {
        PeakMeter {
            decay: (-1.0 / (0.3 * sr)).exp(),
            peak: 0.0,
        }
    }

    pub fn push(&mut self, x: f32) {
        let a = x.abs();
        if a > self.peak {
            self.peak = a;
        } else {
            self.peak *= self.decay;
        }
    }

    pub fn value(&self) -> f32 {
        self.peak
    }

    pub fn reset(&mut self) {
        self.peak = 0.0;
    }
}

/// Equal-power pan: -1 = hard left, 0 = center, +1 = hard right.
pub fn equal_power_pan(pan: f32) -> (f32, f32) {
    let a = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (a.cos(), a.sin())
}

/// One channel strip's parameters (the engine owns the live DSP state).
#[derive(Clone, Debug, PartialEq)]
pub struct StripParams {
    pub trim_db: f32,
    pub polarity: bool,
    pub hpf_freq: f32,
    pub lf_gain: f32,
    pub lmf_gain: f32,
    pub lmf_freq: f32,
    pub hmf_gain: f32,
    pub hmf_freq: f32,
    pub hf_gain: f32,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub pan: f32,
    pub fader: f32,
}

impl Default for StripParams {
    fn default() -> Self {
        StripParams {
            trim_db: 0.0,
            polarity: false,
            hpf_freq: 20.0,
            lf_gain: 0.0,
            lmf_gain: 0.0,
            lmf_freq: 800.0,
            hmf_gain: 0.0,
            hmf_freq: 3000.0,
            hf_gain: 0.0,
            comp_threshold_db: -18.0,
            comp_ratio: 4.0,
            pan: 0.0,
            fader: 1.0,
        }
    }
}

/// Live channel strip: trim + polarity + EQ + compressor.
pub struct Strip {
    eq: Eq,
    comp: Compressor,
    params: StripParams,
}

impl Strip {
    pub fn new(sr: f32) -> Self {
        Strip {
            eq: Eq::new(sr),
            comp: Compressor::new(sr),
            params: StripParams::default(),
        }
    }

    pub fn set_params(&mut self, p: &StripParams) {
        self.eq.set(
            p.hpf_freq,
            p.lf_gain,
            p.lmf_gain,
            p.lmf_freq,
            p.hmf_gain,
            p.hmf_freq,
            p.hf_gain,
        );
        self.comp.set(p.comp_threshold_db, p.comp_ratio);
        self.params = p.clone();
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let mut y = x * db_to_linear(self.params.trim_db);
        if self.params.polarity {
            y = -y;
        }
        y = self.eq.process(y);
        self.comp.process(y)
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.comp.gain_reduction_db()
    }

    pub fn reset(&mut self) {
        self.eq.reset();
        self.comp.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin())
            .collect()
    }

    fn rms(buf: &[f32]) -> f32 {
        (buf.iter().map(|x| x * x).sum::<f32>() / buf.len() as f32).sqrt()
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    /// THD via Goertzel at the fundamental + harmonics 2..5.
    fn thd(buf: &[f32], fund: f32) -> f32 {
        fn goertzel(buf: &[f32], freq: f32) -> f32 {
            let w = 2.0 * std::f32::consts::PI * freq / SR;
            let cw = w.cos();
            let coeff = 2.0 * cw;
            let (mut s0, mut s1, mut s2) = (0.0, 0.0, 0.0);
            for &x in buf {
                s0 = x + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
            (power / (buf.len() as f32 * buf.len() as f32)).sqrt() * 2.0
        }
        let fund_amp = goertzel(buf, fund);
        let mut harm = 0.0;
        for h in 2..=5 {
            let a = goertzel(buf, fund * h as f32);
            harm += a * a;
        }
        harm.sqrt() / fund_amp
    }

    #[test]
    fn lowpass_attenuates_highs_and_passes_dc() {
        let mut lp = Biquad::lowpass(1000.0, 0.7071, SR);
        let hi = sine(10000.0, 0.5, 48000);
        let out: Vec<f32> = hi.iter().map(|&x| lp.process(x)).collect();
        assert!(rms(&out[24000..]) < 0.05, "10k through 1k LP should be small");
        let mut lp = Biquad::lowpass(1000.0, 0.7071, SR);
        let dc: Vec<f32> = (0..4800).map(|_| 1.0).collect();
        let out: Vec<f32> = dc.iter().map(|&x| lp.process(x)).collect();
        assert!((out[4000] - 1.0).abs() < 0.01, "DC should pass the LP");
    }

    #[test]
    fn highpass_blocks_dc() {
        let mut hp = Biquad::highpass(100.0, 0.7071, SR);
        let dc: Vec<f32> = (0..48000).map(|_| 1.0).collect();
        let out: Vec<f32> = dc.iter().map(|&x| hp.process(x)).collect();
        assert!(out[40000].abs() < 0.01, "DC should be blocked by HP");
    }

    #[test]
    fn peaking_boosts_at_center() {
        let mut pk = Biquad::peaking(1000.0, 1.0, 12.0, SR);
        let sig = sine(1000.0, 0.25, 48000);
        let out: Vec<f32> = sig.iter().map(|&x| pk.process(x)).collect();
        let r = rms(&out[24000..]) / rms(&sig[24000..]);
        assert!(r > 3.5 && r < 4.5, "12 dB peaking should ~4x the center, got {r}");
    }

    #[test]
    fn eq_flat_is_near_unity() {
        let mut eq = Eq::new(SR);
        eq.set(20.0, 0.0, 0.0, 800.0, 0.0, 3000.0, 0.0);
        let sig = sine(1000.0, 0.5, 48000);
        let out: Vec<f32> = sig.iter().map(|&x| eq.process(x)).collect();
        let diff = out[24000..]
            .iter()
            .zip(&sig[24000..])
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(diff < 0.02, "flat EQ should pass signal, max diff {diff}");
    }

    #[test]
    fn compressor_reduces_loud_peaks() {
        let mut comp = Compressor::new(SR);
        comp.set(-20.0, 4.0);
        let sig = sine(1000.0, 1.0, 48000);
        let out: Vec<f32> = sig.iter().map(|&x| comp.process(x)).collect();
        assert!(peak(&out[24000..]) < 0.7, "compressed peak should drop");
        assert!(comp.gain_reduction_db() > 5.0, "GR should be significant");
    }

    #[test]
    fn tube_thd_near_one_point_five_percent_at_minus_6db() {
        let sig = sine(1000.0, 0.5, 48000); // -6 dBFS
        let mut dc = DcBlocker::default();
        let out: Vec<f32> = sig.iter().map(|&x| dc.process(tube_drive(x))).collect();
        let t = thd(&out[12000..], 1000.0);
        assert!(
            t > 0.010 && t < 0.020,
            "tube THD at -6 dBFS should be ~1.5%, got {:.3}%",
            t * 100.0
        );
    }

    #[test]
    fn maximiser_raises_rms_without_exceeding_unity() {
        let sig = sine(1000.0, 0.5, 48000);
        let dry = rms(&sig);
        let wet: Vec<f32> = sig.iter().map(|&x| maximiser(x, 1.0)).collect();
        assert!(rms(&wet) > dry, "maximiser should raise RMS");
        assert!(peak(&wet) <= 1.0 + 1e-6, "maximiser must not exceed unity");
        let unity: Vec<f32> = sig.iter().map(|&x| maximiser(x, 0.0)).collect();
        assert!(
            unity.iter().zip(&sig).all(|(a, b)| (a - b).abs() < 1e-6),
            "amount 0 must be unity"
        );
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut lim = Limiter::new(SR, -0.5);
        let mut sig = sine(1000.0, 1.0, 48000);
        for x in sig.iter_mut().take(4800) {
            *x = 2.0; // burst twice full scale
        }
        let out: Vec<f32> = sig.iter().map(|&x| lim.process(x)).collect();
        assert!(
            peak(&out) <= lim.ceiling() + 1e-4,
            "limiter must not exceed ceiling, peak {}",
            peak(&out)
        );
    }

    #[test]
    fn pan_law() {
        let (l, r) = equal_power_pan(-1.0);
        assert!(l > 0.99 && r < 0.01);
        let (l, r) = equal_power_pan(1.0);
        assert!(l < 0.01 && r > 0.99);
        let (l, r) = equal_power_pan(0.0);
        assert!((l - r).abs() < 1e-6 && (l - 0.7071).abs() < 0.01);
    }
}
