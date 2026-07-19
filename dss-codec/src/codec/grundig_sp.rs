//! Grundig DSS-SP (PH9607) CELP decoder.
//!
//! Clean reimplementation derived from the Grundig DigtaSoft reference decoder
//! (dss2wav.dll, DigtaSoft One). Decodes the Grundig `\x06dss` variant of the
//! Digital Speech Standard. The synthesis runs a 14th-order reflection-coefficient
//! lattice driven by an adaptive (pitch) plus fixed (algebraic) codebook
//! excitation at 12 kHz (288 samples/frame), then a 3:4 polyphase FIR resampler
//! lifts it to 16 kHz (384 samples/frame).
//!
//! The output is bit-exact with the reference codec on the Grundig sample set.

use crate::tables::grundig_sp::*;

const SUB: usize = 4; // subframes per frame
const SUBLEN: usize = 72; // samples per subframe @ 12 kHz
const ORDER: usize = 14; // LPC / lattice order
const F318: f64 = -0.1; // de-emphasis pole

const F340: f64 = 32767.5;
const F348: f64 = -32767.5;
const F350: f64 = -0.5;

/// Round-and-clamp identical to the reference: values in [-32767.5, 32767.5]
/// are `floor(x + 0.5)`, the upper tail saturates to 32767, the lower tail to
/// -32767 (the asymmetric lower bound matches the original).
#[inline]
fn round_clamp(x: f64) -> i64 {
    if x <= F340 {
        if x >= F348 {
            (x - F350).floor() as i64
        } else {
            -32767
        }
    } else {
        32767
    }
}

/// Per-frame CELP synthesizer state, carried across the whole file.
pub struct GrundigSpDecoder {
    hist: [f64; 188], // excitation history (read base index 187, write @115)
    syn: [f64; 16],   // lattice b-state (order 14)
    rnd: u32,         // unvoiced PRNG (16-bit)
    deemph: f64,      // de-emphasis memory
    pcm12: Vec<f64>,  // 12 kHz PCM accumulated across frames
}

impl Default for GrundigSpDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GrundigSpDecoder {
    pub fn new() -> Self {
        Self {
            hist: [0.0; 188],
            syn: [0.0; 16],
            rnd: 0,
            deemph: 0.0,
            pcm12: Vec::new(),
        }
    }

    /// Decode one 41-byte (328-bit) frame. The 12 kHz PCM is buffered; the
    /// final 16 kHz output is produced by `finish()` (the polyphase resampler
    /// needs the whole utterance, matching the reference decoder).
    pub fn decode_frame(&mut self, frame: &[u8]) -> Vec<i64> {
        let bits = unpack_bits(frame);
        let synth = self.decode_frame_pcm12(&bits);
        self.pcm12.extend(synth.iter().map(|&v| v as f64));
        Vec::new()
    }

    /// Resample the accumulated 12 kHz PCM to 16 kHz and return all samples.
    pub fn finish(&mut self) -> Vec<i64> {
        resample(&self.pcm12)
    }

    // -------------------------------------------------- stage 2 (CELP @ 12 kHz)
    fn decode_frame_pcm12(&mut self, bits: &[u8]) -> Vec<i64> {
        // Grundig files only use codec mode 0 (PH9607 standard); mode is fixed.
        const MODE: usize = 0;
        let widths = WIDTHS[MODE];
        let lpc = LPC[MODE];

        let mut fr = FrameReader::new(bits);

        // 14 reflection-coefficient indices (mode 0: voiced is implicit).
        let mut lsf = [0usize; ORDER];
        for (i, slot) in lsf.iter_mut().enumerate() {
            *slot = fr.read(widths[i]) as usize;
        }

        // Per-subframe excitation parameters.
        let mut gains = [0usize; SUB];
        let mut fixed31 = [0u32; SUB];
        let mut pulses = [[0usize; 8]; SUB];
        for sf in 0..SUB {
            gains[sf] = fr.read(5) as usize;
            fixed31[sf] = fr.read(31) as u32;
            pulses[sf][0] = fr.read(6) as usize;
            for k in 1..8 {
                pulses[sf][k] = fr.read(3) as usize;
            }
        }

        // 24-bit differential pitch field -> base-151 / base-48 lags.
        let mut v = fr.read(24);
        let mut pit = [0u32; SUB];
        pit[0] = (v % 0x97) as u32;
        v /= 0x97;
        for slot in pit.iter_mut().skip(1) {
            *slot = (v % 0x30) as u32;
            v /= 0x30;
        }

        let mut refl = [0.0f64; ORDER];
        for (i, slot) in refl.iter_mut().enumerate() {
            *slot = lpc[lsf[i] + 32 * i];
        }

        let mut synth: Vec<f64> = Vec::with_capacity(SUB * SUBLEN);
        let mut prev_lag: u32 = 0;
        for sf in 0..SUB {
            let lag = if sf == 0 {
                pit[0] + 0x24
            } else if prev_lag < 0xa3 {
                let base = prev_lag.saturating_sub(0x17).max(0x24);
                pit[sf] + base
            } else {
                pit[sf] + 0x8b
            };
            prev_lag = lag;
            let exc = self.build_voiced(lag as usize, gains[sf], fixed31[sf], &pulses[sf]);
            let out = self.lattice(&exc, &refl);
            synth.extend_from_slice(&out);
        }

        self.deemph(&synth)
    }

    fn build_voiced(
        &mut self,
        lag: usize,
        gain_idx: usize,
        fixed31: u32,
        pulse8: &[usize; 8],
    ) -> [f64; SUBLEN] {
        let mut out = [0.0f64; SUBLEN];
        let g = PITCHGAIN[gain_idx];

        // Adaptive (pitch) contribution: repeat the excitation history at the
        // pitch lag, scaled by the pitch gain.
        let mut j = 0usize;
        let mut sv = 1usize;
        while j < SUBLEN {
            let hi = sv * lag;
            while j < hi && j < SUBLEN {
                out[j] = self.hist[187 + j - sv * lag] * g;
                j += 1;
            }
            sv += 1;
        }

        // Fixed (algebraic) codebook: 31-bit index -> 7 pulse positions via the
        // cumulative binomial table, signed amplitudes scaled by a lead gain.
        let mut p: i64 = fixed31 as i64;
        if p > 0x57cddec7 {
            p = 0x57cddec7;
        }
        let mut pos = [0usize; 7];
        let mut col: i64 = 0x47;
        let mut row: i64 = 0x1f8;
        for slot in pos.iter_mut() {
            while p < CUM[(col + row) as usize] {
                col -= 1;
            }
            *slot = col as usize;
            p -= CUM[(col + row) as usize];
            row -= 0x48;
        }
        let lead = T1C8[pulse8[0]];
        for k in 0..7 {
            out[pos[k]] += T3C8[pulse8[1 + k]] * lead;
        }

        self.push_hist(&out);
        out
    }

    #[allow(dead_code)]
    fn build_unvoiced(&mut self, gain_idx: usize) -> [f64; SUBLEN] {
        // Present for completeness; Grundig PH9607 streams are all voiced (mode 0).
        let mut out = [0.0f64; SUBLEN];
        let sc = T2908[gain_idx];
        for slot in out.iter_mut() {
            self.rnd = (self.rnd.wrapping_mul(0x209).wrapping_add(0x103)) & 0xffff;
            let s = if self.rnd >= 0x8000 {
                self.rnd as i64 - 0x10000
            } else {
                self.rnd as i64
            };
            *slot = s as f64 * sc;
        }
        self.push_hist(&out);
        out
    }

    #[inline]
    fn push_hist(&mut self, out: &[f64; SUBLEN]) {
        for i in 0..115 {
            self.hist[i] = self.hist[i + 72];
        }
        for i in 0..72 {
            self.hist[115 + i] = out[i];
        }
    }

    fn lattice(&mut self, exc: &[f64; SUBLEN], refl: &[f64; ORDER]) -> [f64; SUBLEN] {
        let mut out = [0.0f64; SUBLEN];
        let b = &mut self.syn;
        for n in 0..SUBLEN {
            let mut f = exc[n] - refl[ORDER - 1] * b[ORDER - 1];
            let mut m = ORDER as i64 - 2;
            while m >= 0 {
                let mi = m as usize;
                f -= refl[mi] * b[mi];
                b[mi + 1] = refl[mi] * f + b[mi];
                m -= 1;
            }
            b[0] = f;
            out[n] = f;
        }
        out
    }

    fn deemph(&mut self, synth: &[f64]) -> Vec<i64> {
        let mut out = Vec::with_capacity(synth.len());
        let mut prev = self.deemph;
        for &x in synth {
            let y = x - prev * F318;
            prev = y;
            out.push(round_clamp(y));
        }
        self.deemph = prev;
        out
    }
}

// -------------------------------------------------- stage 3 (3:4 polyphase FIR)
//
// 12 kHz -> 16 kHz. 100 taps per phase, plus a single extra tap on phase 0.
// `HIST` (0xce) zero-history samples precede the first input sample.
fn resample(inp: &[f64]) -> Vec<i64> {
    const HIST: usize = 0xce;
    let mut x = vec![0.0f64; HIST];
    x.extend_from_slice(inp);
    let n = x.len();
    let mut out = Vec::new();
    let mut uv: i64 = 0;
    let mut pos: usize = HIST;
    while pos < n {
        let mut acc = 0.0f64;
        let mut c = uv as usize;
        for k in 0..100 {
            acc += x[pos - k] * RESAMP[c];
            c += 4;
        }
        if uv == 0 {
            acc += x[pos - 100] * RESAMP[400];
        }
        out.push(round_clamp(acc));
        uv += 3;
        if uv > 3 {
            let adv = uv >> 2;
            pos += adv as usize;
            uv -= 4 * adv;
        }
    }
    out
}

// ------------------------------------------------------------------ bit I/O

/// Unpack a 41-byte CELP frame into 328 MSB-first bits.
fn unpack_bits(frame: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(328);
    for &byte in frame.iter().take(41) {
        for k in (0..8).rev() {
            bits.push((byte >> k) & 1);
        }
    }
    while bits.len() < 328 {
        bits.push(0);
    }
    bits.truncate(328);
    bits
}

struct FrameReader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> FrameReader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn read(&mut self, n: usize) -> u64 {
        let mut v: u64 = 0;
        for _ in 0..n {
            let bit = if self.i < self.b.len() {
                self.b[self.i] as u64
            } else {
                0
            };
            v = (v << 1) | bit;
            self.i += 1;
        }
        v
    }
}
