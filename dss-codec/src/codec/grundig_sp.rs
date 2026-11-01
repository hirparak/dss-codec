//! Grundig DSS-SP (PH9607) CELP decoder.
//!
//! Clean reimplementation derived from the Grundig DigtaSoft reference decoder
//! (dss2wav.dll, DigtaSoft One). Decodes the Grundig `\x06dss` variant of the
//! Digital Speech Standard. The synthesis runs a 14th-order reflection-coefficient
//! lattice driven by an adaptive (pitch) plus fixed (algebraic) codebook
//! excitation at 12 kHz and outputs 288 samples per frame at that rate.
//!
//! Grundig DigtaSoft One lifts the 12 kHz signal to 16 kHz with a 3:4
//! polyphase FIR. That FIR is not part of the codec, so this decoder stops
//! at 12 kHz. Through that FIR the output is bit-exact with DigtaSoft One's
//! 16 kHz output on the Grundig sample set.
//!
//! Olympus DSS and DS2 SP use the same frames. Their format 1 (variable
//! rate) adds a leading type bit. A long frame (type 1) has a 2-bit last
//! filter index with its own 4-entry table. A short frame (type 0) has 72
//! bits: the type bit, the 14 filter indices and four 5-bit noise gains.

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
    variable_rate: bool,
}

impl Default for GrundigSpDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GrundigSpDecoder {
    pub fn new() -> Self {
        Self::with_variable_rate(false)
    }

    /// A decoder for DSS format 1 frames when `variable_rate` is set.
    pub fn with_variable_rate(variable_rate: bool) -> Self {
        Self {
            hist: [0.0; 188],
            syn: [0.0; 16],
            rnd: 0,
            deemph: 0.0,
            variable_rate,
        }
    }

    /// Decode one 41-byte (328-bit) frame into 288 samples at 12 kHz.
    pub fn decode_frame(&mut self, frame: &[u8]) -> Vec<i64> {
        let bits = unpack_bits(frame);
        self.decode_frame_pcm12(&bits)
    }

    /// Decode one packet from the DSS SP demuxer into 288 samples at 12 kHz.
    /// The packet keeps the file's 16-bit little-endian words, and the frame
    /// bits run MSB first through each word, so each byte pair is swapped.
    pub fn decode_dss_packet(&mut self, pkt: &[u8]) -> Vec<i64> {
        let mut frame = [0u8; 42];
        for (i, &byte) in pkt.iter().take(frame.len()).enumerate() {
            frame[i ^ 1] = byte;
        }
        self.decode_frame(&frame)
    }

    // -------------------------------------------------- stage 2 (CELP @ 12 kHz)
    fn decode_frame_pcm12(&mut self, bits: &[u8]) -> Vec<i64> {
        let mode = usize::from(self.variable_rate);
        let widths = WIDTHS[mode];
        let lpc = LPC[mode];

        let mut fr = FrameReader::new(bits);
        let noise = self.variable_rate && fr.read(1) == 0;

        // 14 reflection-coefficient indices.
        let mut lsf = [0usize; ORDER];
        for (i, slot) in lsf.iter_mut().enumerate() {
            *slot = fr.read(widths[i]) as usize;
        }

        let mut refl = [0.0f64; ORDER];
        for (i, slot) in refl.iter_mut().enumerate() {
            *slot = lpc[lsf[i] + 32 * i];
        }

        if noise {
            let mut synth: Vec<f64> = Vec::with_capacity(SUB * SUBLEN);
            for _ in 0..SUB {
                let exc = self.build_unvoiced(fr.read(5) as usize);
                let out = self.lattice(&exc, &refl);
                synth.extend_from_slice(&out);
            }
            return self.deemph(&synth);
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

    fn build_unvoiced(&mut self, gain_idx: usize) -> [f64; SUBLEN] {
        let mut out = [0.0f64; SUBLEN];
        let sc = T2908[gain_idx];
        for slot in out.iter_mut() {
            self.rnd = (self.rnd.wrapping_mul(0x209).wrapping_add(0x103)) & 0xffff;
            let s = if self.rnd >= 0x8000 {
                self.rnd as i64 - 0x10000
            } else {
                self.rnd as i64
            };
            *slot = s as f64 * sc * std::f64::consts::SQRT_2;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const LSF: [u64; ORDER] = [9, 17, 3, 12, 5, 8, 2, 14, 6, 1, 4, 7, 0, 3];
    const GAINS: [u64; SUB] = [20, 25, 31, 12];
    const NOISE_GAIN: [u32; 32] = [
        0, 5, 10, 15, 21, 28, 35, 42, 50, 59, 69, 79, 90, 102, 115, 129, 144, 160, 178, 197,
        218, 240, 264, 290, 318, 348, 381, 417, 455, 496, 541, 589,
    ];
    const LAST_INDEX: [f64; 4] = [
        -0.2492040991783142,
        -0.07383912801742554,
        0.06094199046492577,
        0.204614520072937,
    ];

    fn pack(fields: &[(u64, usize)]) -> [u8; 42] {
        let mut out = [0u8; 42];
        let mut pos = 0usize;
        for &(value, width) in fields {
            for k in (0..width).rev() {
                if (value >> k) & 1 == 1 {
                    out[pos / 8] |= 0x80 >> (pos % 8);
                }
                pos += 1;
            }
        }
        out
    }

    fn short_frame() -> [u8; 42] {
        let mut fields = vec![(0, 1)];
        fields.extend(LSF.iter().zip(WIDTHS1.iter()).map(|(&v, &w)| (v, w)));
        fields.extend(GAINS.iter().map(|&g| (g, 5)));
        pack(&fields)
    }

    fn lcg(state: &mut u32) -> f64 {
        *state = (*state * 0x209 + 0x103) & 0xffff;
        f64::from(*state as u16 as i16)
    }

    #[test]
    fn noise_runs_one_signed_seed_through_the_gain_table() {
        let mut dec = GrundigSpDecoder::with_variable_rate(true);
        let mut state = 0u32;
        for gain in [31usize, 7, 0, 18, 1] {
            let exc = dec.build_unvoiced(gain);
            for &x in exc.iter() {
                let expected = lcg(&mut state) * f64::from(NOISE_GAIN[gain]) / 32768.0
                    * std::f64::consts::SQRT_2;
                assert_eq!(x, expected);
            }
        }
    }

    #[test]
    fn short_frame_takes_the_last_filter_index_from_the_format_1_table() {
        let mut refl = [0.0f64; ORDER];
        for (i, slot) in refl.iter_mut().enumerate().take(ORDER - 1) {
            *slot = LPC0[LSF[i] as usize + 32 * i];
        }
        refl[ORDER - 1] = LAST_INDEX[LSF[ORDER - 1] as usize];
        let mut reference = GrundigSpDecoder::with_variable_rate(true);
        let mut synth = Vec::new();
        for &gain in GAINS.iter() {
            let exc = reference.build_unvoiced(gain as usize);
            synth.extend_from_slice(&reference.lattice(&exc, &refl));
        }
        let expected = reference.deemph(&synth);

        let out = GrundigSpDecoder::with_variable_rate(true).decode_frame(&short_frame());
        assert_eq!(out.len(), SUB * SUBLEN);
        assert!(out.iter().any(|&s| s != 0));
        assert_eq!(out, expected);
    }

    #[test]
    fn short_frame_after_a_long_frame_uses_its_own_filter() {
        let mut refl = [0.0f64; ORDER];
        for (i, slot) in refl.iter_mut().enumerate().take(ORDER - 1) {
            *slot = LPC0[LSF[i] as usize + 32 * i];
        }
        refl[ORDER - 1] = LAST_INDEX[LSF[ORDER - 1] as usize];
        let mut long = [0x5au8; 42];
        long[0] |= 0x80;

        let mut reference = GrundigSpDecoder::with_variable_rate(true);
        reference.decode_frame(&long);
        let mut synth = Vec::new();
        for &gain in GAINS.iter() {
            let exc = reference.build_unvoiced(gain as usize);
            synth.extend_from_slice(&reference.lattice(&exc, &refl));
        }
        let expected = reference.deemph(&synth);

        let mut dec = GrundigSpDecoder::with_variable_rate(true);
        dec.decode_frame(&long);
        let out = dec.decode_frame(&short_frame());
        assert!(out.iter().any(|&s| s != 0));
        assert_eq!(out, expected);
    }

    #[test]
    fn short_frame_ends_after_72_bits() {
        let frame = short_frame();
        let mut padded = frame;
        padded[9..].fill(0xff);
        let a = GrundigSpDecoder::with_variable_rate(true).decode_frame(&frame);
        let b = GrundigSpDecoder::with_variable_rate(true).decode_frame(&padded);
        assert_eq!(a, b);
    }

    #[test]
    fn noise_frames_leave_the_pitch_history_alone() {
        let mut dec = GrundigSpDecoder::with_variable_rate(true);
        let mut long = [0x5au8; 42];
        long[0] |= 0x80;
        dec.decode_frame(&long);
        let hist = dec.hist;
        assert!(hist.iter().any(|&x| x != 0.0));
        let mut state = 0u32;
        for _ in 0..2 {
            dec.decode_frame(&short_frame());
            for _ in 0..SUB * SUBLEN {
                lcg(&mut state);
            }
            assert_eq!(dec.hist, hist);
            assert_eq!(dec.rnd, state);
        }
    }

    #[test]
    fn fixed_rate_frames_have_no_type_bit() {
        let frame = [0x5au8; 42];
        let mut other = frame;
        other[12..].fill(0xa5);
        let a = GrundigSpDecoder::new().decode_frame(&frame);
        let b = GrundigSpDecoder::new().decode_frame(&other);
        assert_ne!(a, b);
    }

    #[test]
    fn dss_packet_swaps_each_byte_pair() {
        let pkt: Vec<u8> = (0..42u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut frame = [0u8; 42];
        for (i, pair) in pkt.chunks(2).enumerate() {
            frame[2 * i] = pair[1];
            frame[2 * i + 1] = pair[0];
        }
        for variable_rate in [false, true] {
            let a = GrundigSpDecoder::with_variable_rate(variable_rate).decode_dss_packet(&pkt);
            let b = GrundigSpDecoder::with_variable_rate(variable_rate).decode_frame(&frame);
            assert!(a.iter().any(|&s| s != 0));
            assert_eq!(a, b);
        }
    }
}
