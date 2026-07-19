//! DSS SP decoder — Q15 integer arithmetic matching FFmpeg dss_sp.c / DssDecoder.dll.
//!
//! Architecture: CELP with 14 reflection coefficients, Levinson recursion,
//! pitch-adaptive excitation, 7-pulse fixed codebook, cascaded LPC synthesis +
//! error correction, noise modulation, and 11:12 sinc resampling (12000→11025 Hz).

use crate::bitstream::BitstreamReader;
use crate::tables::dss_sp::*;

const SUBFRAMES: usize = 4;
const SUBFRAME_SIZE: usize = 72;
const OUTPUT_SAMPLES: usize = 264;

fn clip16(x: i64) -> i64 {
    x.clamp(-32768, 32767)
}

/// FFmpeg dss_sp_clip_sinc_pcm: values in [-32768, 32767]
/// pass through; anything beyond clamps to +-32767. This preserves -32768
/// (unlike a plain +-32767 clamp) and maps out-of-range negatives to -32767
/// (unlike av_clip_int16). It is the decoder's internal clip everywhere, and
/// also the output clip on repack (NCH) streams.
fn clip_sinc_pcm(x: i64) -> i64 {
    if (-32768..=32767).contains(&x) {
        x
    } else if x <= 0 {
        -32767
    } else {
        32767
    }
}

/// DSS_SP_FORMULA: ((a * 32768 + b * c) + 16384) >> 15
fn formula(a: i64, b: i64, c: i64) -> i64 {
    ((a * 32768 + b * c) + 16384) >> 15
}

/// Combinatorial-table pulse-position decode (FFmpeg dss_sp_decode_pulse_pos_d32
/// / the pulse_dec_mode table path — both use the same table numerically).
fn decode_pulse_pos_table(cp: i64, pulse_pos: &mut [usize; 7]) {
    let mut pulse = 7usize;
    let mut pulse_idx = 71usize;
    let mut c = cp;
    for slot in pulse_pos.iter_mut() {
        while c < COMBINATORIAL_TABLE[pulse][pulse_idx] {
            if pulse_idx == 0 {
                break;
            }
            pulse_idx -= 1;
        }
        c -= COMBINATORIAL_TABLE[pulse][pulse_idx];
        pulse -= 1;
        *slot = pulse_idx;
    }
}

/// Alternate placement decode (FFmpeg dss_sp_decode_pulse_pos_alt). Succeeds
/// only when all 7 pulses place and the remainder is exactly 0.
///
/// Uses u32 wrapping arithmetic to match FFmpeg's `unsigned int` exactly: the
/// Pascal-triangle update can underflow, and u32 wrap (vs a signed value going
/// negative) flips the `<=` comparison, changing placements for some cp values.
fn decode_pulse_pos_alt(cp: i64, pulse_pos: &mut [usize; 7]) -> bool {
    let mut c72: [u32; 8] = [
        72, 2556, 59640, 1028790, 13991544, 156238908, 1473109704, 3379081753,
    ];
    let mut index = 6usize;
    let mut placements = 0;
    pulse_pos[6] = 0;
    let mut combined = cp as u32;
    for i in (0..=71i32).rev() {
        if c72[index] <= combined {
            combined = combined.wrapping_sub(c72[index]);
            pulse_pos[6 - index] = i as usize;
            placements += 1;
            if index == 0 {
                break;
            }
            index -= 1;
        }
        c72[0] = c72[0].wrapping_sub(1);
        if index > 0 {
            for a in 0..index {
                c72[a + 1] = c72[a + 1].wrapping_sub(c72[a]);
            }
        }
    }
    combined == 0 && placements == 7
}

struct SubframeParams {
    combined_pulse_pos: i64,
    gain: usize,
    pulse_val: [usize; 7],
    pulse_pos: [usize; 7],
}

pub struct DssSpDecoder {
    excitation: Vec<i64>,
    history: Vec<i64>,
    working_buffer: [[i64; SUBFRAME_SIZE]; SUBFRAMES],
    audio_buf: [i64; 15],
    err_buf1: [i64; 15],
    err_buf2: [i64; 15],
    lpc_filter: [i64; 14],
    filter: [i64; 15],
    vector_buf: [i64; SUBFRAME_SIZE],
    noise_state: i64,
    pulse_dec_mode: bool,
    shift_amount: i32,
    /* Pulse-decode routing, matching FFmpeg dss_sp.c.
     * comb_pulse_mode: frame-0 flag (b0&0x80 ? b0&1 : 0).
     * repack: per-frame; comb streams read bit7 of byte1, else always 1.
     * repack_pulse_tbl: sticky table-decode latch (cleared only on cp clamp). */
    comb_pulse_mode: bool,
    repack: bool,
    repack_pulse_tbl: bool,
    frame_idx: usize,
}

impl Default for DssSpDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DssSpDecoder {
    pub fn new() -> Self {
        Self {
            excitation: vec![0; 288 + 6],
            history: vec![0; 187],
            working_buffer: [[0; SUBFRAME_SIZE]; SUBFRAMES],
            audio_buf: [0; 15],
            err_buf1: [0; 15],
            err_buf2: [0; 15],
            lpc_filter: [0; 14],
            filter: [0; 15],
            vector_buf: [0; SUBFRAME_SIZE],
            noise_state: 0,
            pulse_dec_mode: true,
            shift_amount: 0,
            comb_pulse_mode: false,
            repack: false,
            repack_pulse_tbl: false,
            frame_idx: 0,
        }
    }

    pub fn decode_frame(&mut self, pkt: &[u8]) -> Vec<i16> {
        let (filter_idx, sf_adaptive_gain, pitch_lag, subframes) = self.unpack_coeffs(pkt);

        self.unpack_filter(&filter_idx);
        self.convert_coeffs();

        for j in 0..SUBFRAMES {
            self.gen_exc(pitch_lag[j], ADAPTIVE_GAIN[sf_adaptive_gain[j]] as i64);
            self.add_pulses(&subframes[j]);
            self.update_buf();

            for i in 0..SUBFRAME_SIZE {
                self.vector_buf[i] = self.history[SUBFRAME_SIZE - i];
            }

            // shift_sq_sub with err_buf2
            {
                let shift = 13 - self.shift_amount;
                for a in 0..SUBFRAME_SIZE {
                    let mut tmp = self.vector_buf[a] * self.filter[0];
                    for i in (1..=14).rev() {
                        tmp -= self.err_buf2[i] * self.filter[i];
                    }
                    for i in (1..=14).rev() {
                        self.err_buf2[i] = self.err_buf2[i - 1];
                    }
                    tmp = (tmp + 4096) >> shift;
                    self.err_buf2[1] = clip_sinc_pcm(tmp);
                    self.vector_buf[a] = clip_sinc_pcm(tmp);
                }
            }

            self.sf_synthesis(self.lpc_filter[0], j);
        }

        // Flatten working buffer
        let mut working_flat = [0i64; 288];
        for j in 0..SUBFRAMES {
            working_flat[j * SUBFRAME_SIZE..(j + 1) * SUBFRAME_SIZE]
                .copy_from_slice(&self.working_buffer[j]);
        }

        let out = self.update_state(&working_flat);
        self.frame_idx += 1;
        out
    }

    fn unpack_coeffs(
        &mut self,
        pkt: &[u8],
    ) -> (Vec<usize>, Vec<usize>, Vec<usize>, Vec<SubframeParams>) {
        // Stream routing: comb_pulse_mode is a frame-0 flag; repack
        // is per-packet on comb streams (bit7 of byte1) and always 1 otherwise.
        if self.frame_idx == 0 {
            self.comb_pulse_mode = (pkt[0] & 0x80) != 0 && (pkt[0] & 1) != 0;
        }
        self.repack = if self.comb_pulse_mode {
            (pkt[1] >> 7) & 1 != 0
        } else {
            true
        };

        let mut reader = BitstreamReader::new(pkt);

        // Reflection coefficient indices: 2x5 + 6x4 + 6x3 = 52 bits
        let mut filter_idx = Vec::with_capacity(14);
        for _ in 0..2 {
            filter_idx.push(reader.read_bits(5) as usize);
        }
        for _ in 0..6 {
            filter_idx.push(reader.read_bits(4) as usize);
        }
        for _ in 0..6 {
            filter_idx.push(reader.read_bits(3) as usize);
        }

        // Per-subframe: 5 + 31 + 6 + 7*3 = 63 bits x 4 = 252 bits
        let mut sf_adaptive_gain = Vec::with_capacity(SUBFRAMES);
        let mut subframes = Vec::with_capacity(SUBFRAMES);

        for _ in 0..SUBFRAMES {
            let ag = reader.read_bits(5) as usize;
            sf_adaptive_gain.push(ag);

            let combined_pulse_pos = reader.read_bits(31) as i64;
            let gain = reader.read_bits(6) as usize;
            let mut pulse_val = [0usize; 7];
            for pv in &mut pulse_val {
                *pv = reader.read_bits(3) as usize;
            }
            subframes.push(SubframeParams {
                combined_pulse_pos,
                gain,
                pulse_val,
                pulse_pos: [0; 7],
            });
        }

        // Decode pulse positions. Mirrors FFmpeg dss_sp.c: comb
        // streams (repack==0) use the plain combinatorial table; repack streams
        // (0a8/DS23 clean CELP; some 4bed frames) try an alternate placement
        // decode first and fall back to the table via a sticky latch.
        for j in 0..SUBFRAMES {
            let combined = subframes[j].combined_pulse_pos;
            if combined < C72_BINOMIALS[7] {
                if self.repack {
                    let mut cp = combined;
                    if cp > C72_BINOMIALS[6] - 1 {
                        cp = C72_BINOMIALS[6] - 1;
                        self.pulse_dec_mode = false;
                        self.repack_pulse_tbl = false;
                    }
                    if self.repack_pulse_tbl {
                        decode_pulse_pos_table(cp, &mut subframes[j].pulse_pos);
                    } else {
                        let mut alt = [0usize; 7];
                        if decode_pulse_pos_alt(cp, &mut alt) {
                            subframes[j].pulse_pos = alt;
                        } else {
                            self.repack_pulse_tbl = true;
                            decode_pulse_pos_table(cp, &mut subframes[j].pulse_pos);
                        }
                    }
                } else if self.pulse_dec_mode {
                    decode_pulse_pos_table(combined, &mut subframes[j].pulse_pos);
                }
            } else {
                self.pulse_dec_mode = false;
                let mut c72 = C72_BINOMIALS;
                subframes[j].pulse_pos[6] = 0;
                let mut index = 6usize;
                let mut cp = combined;
                for i in (0..=71i32).rev() {
                    if c72[index] <= cp {
                        cp -= c72[index];
                        subframes[j].pulse_pos[6 - index] = i as usize;
                        if index == 0 {
                            break;
                        }
                        index -= 1;
                    }
                    c72[0] -= 1;
                    if index > 0 {
                        for a in 0..index {
                            c72[a + 1] -= c72[a];
                        }
                    }
                }
            }
        }

        // Combined pitch (24 bits)
        let combined_pitch = reader.read_bits(24) as u64;

        let mut pitch_lag = vec![0usize; SUBFRAMES];
        pitch_lag[0] = ((combined_pitch % 151) + 36) as usize;
        let mut cp = combined_pitch / 151;

        for i in 1..SUBFRAMES - 1 {
            pitch_lag[i] = (cp % 48) as usize;
            cp /= 48;
        }
        pitch_lag[SUBFRAMES - 1] = cp.min(47) as usize;

        // Convert delta pitch to absolute
        let mut pl = pitch_lag[0];
        for i in 1..SUBFRAMES {
            if pl > 162 {
                pitch_lag[i] += 162 - 23;
            } else {
                let tmp = pl.saturating_sub(23);
                let tmp = tmp.max(36);
                pitch_lag[i] += tmp;
            }
            pl = pitch_lag[i];
        }

        (filter_idx, sf_adaptive_gain, pitch_lag, subframes)
    }

    fn unpack_filter(&mut self, filter_idx: &[usize]) {
        for i in 0..14 {
            self.lpc_filter[i] = FILTER_CB[i][filter_idx[i]] as i64;
        }
    }

    fn convert_coeffs(&mut self) {
        self.shift_amount = 0;
        self.filter[0] = 0x2000;
        let mut overflow = false;

        for a in 0..14 {
            let a_plus = a + 1;
            self.filter[a_plus] = self.lpc_filter[a] >> 2;
            for i in 1..=(a_plus / 2) {
                let coeff_1 = self.filter[i];
                let coeff_2 = self.filter[a_plus - i];
                let tmp1 = formula(coeff_1, self.lpc_filter[a], coeff_2);
                let tmp2 = formula(coeff_2, self.lpc_filter[a], coeff_1);
                if !(-32768..=32767).contains(&tmp1) || !(-32768..=32767).contains(&tmp2) {
                    overflow = true;
                }
                self.filter[i] = clip16(tmp1);
                self.filter[a_plus - i] = clip16(tmp2);
            }
        }

        if overflow {
            self.shift_amount = 1;
            self.filter[0] = 0x1000;
            for a in 0..14 {
                let a_plus = a + 1;
                self.filter[a_plus] = self.lpc_filter[a] >> 3;
                for i in 1..=(a_plus / 2) {
                    let coeff_1 = self.filter[i];
                    let coeff_2 = self.filter[a_plus - i];
                    self.filter[i] = clip16(formula(coeff_1, self.lpc_filter[a], coeff_2));
                    self.filter[a_plus - i] =
                        clip16(formula(coeff_2, self.lpc_filter[a], coeff_1));
                }
            }
        }
    }

    fn gen_exc(&mut self, pitch_lag: usize, gain: i64) {
        if pitch_lag < SUBFRAME_SIZE {
            for i in 0..SUBFRAME_SIZE {
                self.vector_buf[i] = self.history[pitch_lag - i % pitch_lag];
            }
        } else {
            for i in 0..SUBFRAME_SIZE {
                self.vector_buf[i] = self.history[pitch_lag - i];
            }
        }

        for i in 0..SUBFRAME_SIZE {
            let tmp = (gain * self.vector_buf[i]) >> 11;
            self.vector_buf[i] = clip_sinc_pcm(tmp);
        }
    }

    fn add_pulses(&mut self, sf: &SubframeParams) {
        for i in 0..7 {
            let pos = sf.pulse_pos[i];
            let val =
                (FIXED_CB_GAIN[sf.gain] as i64 * PULSE_VAL[sf.pulse_val[i]] as i64 + 0x4000)
                    >> 15;
            self.vector_buf[pos] += val;
        }
    }

    fn update_buf(&mut self) {
        for i in (1..=114).rev() {
            self.history[i + SUBFRAME_SIZE] = self.history[i];
        }
        for i in 0..SUBFRAME_SIZE {
            self.history[SUBFRAME_SIZE - i] = self.vector_buf[i];
        }
    }

    fn sf_synthesis(&mut self, lpc_filter_0: i64, subframe_idx: usize) {
        let size = SUBFRAME_SIZE;

        let vsum_1 = {
            let s: i64 = self.vector_buf[..size].iter().map(|v| v.abs()).sum();
            s.min(0xFFFFF)
        };

        let normalize_bits = {
            let mut val: i64 = 1;
            for v in &self.vector_buf[..size] {
                val |= v.abs();
            }
            let mut nb = 0i32;
            while val <= 0x4000 {
                val *= 2;
                nb += 1;
            }
            nb
        };

        // Scale up
        scale_vec(&mut self.vector_buf, normalize_bits - 3, size);
        scale_vec_arr(&mut self.audio_buf, normalize_bits, 15);
        scale_vec_arr(&mut self.err_buf1, normalize_bits, 15);

        let v36 = self.err_buf1[1];

        // shift_sq_add with BINARY_DECREASING
        {
            let tmp_buf = vec_mult(&self.filter, &BINARY_DECREASING);
            let shift = 13 - self.shift_amount;
            for a in 0..size {
                self.audio_buf[0] = self.vector_buf[a];
                let mut tmp: i64 = 0;
                for i in (0..=14).rev() {
                    tmp += self.audio_buf[i] * tmp_buf[i];
                }
                for i in (1..=14).rev() {
                    self.audio_buf[i] = self.audio_buf[i - 1];
                }
                tmp = (tmp + 4096) >> shift;
                self.vector_buf[a] = clip_sinc_pcm(tmp);
            }
        }

        // shift_sq_sub with UNC_DECREASING
        {
            let tmp_buf = vec_mult(&self.filter, &UNC_DECREASING);
            let shift = 13 - self.shift_amount;
            for a in 0..size {
                let mut tmp = self.vector_buf[a] * tmp_buf[0];
                for i in (1..=14).rev() {
                    tmp -= self.err_buf1[i] * tmp_buf[i];
                }
                for i in (1..=14).rev() {
                    self.err_buf1[i] = self.err_buf1[i - 1];
                }
                tmp = (tmp + 4096) >> shift;
                self.err_buf1[1] = clip_sinc_pcm(tmp);
                self.vector_buf[a] = clip_sinc_pcm(tmp);
            }
        }

        // Noise modulation LPC
        let lf = {
            let half = lpc_filter_0 >> 1;
            if half >= 0 { 0 } else { half }
        };

        if size > 1 {
            for i in (1..size).rev() {
                let tmp = formula(self.vector_buf[i], lf, self.vector_buf[i - 1]);
                self.vector_buf[i] = clip_sinc_pcm(tmp);
            }
        }
        {
            let tmp = formula(self.vector_buf[0], lf, v36);
            self.vector_buf[0] = clip_sinc_pcm(tmp);
        }

        // Scale down
        scale_vec(&mut self.vector_buf, -normalize_bits, size);
        scale_vec_arr(&mut self.audio_buf, -normalize_bits, 15);
        scale_vec_arr(&mut self.err_buf1, -normalize_bits, 15);

        // Energy ratio and noise generation
        let vsum_2: i64 = self.vector_buf[..size].iter().map(|v| v.abs()).sum();
        let t = if vsum_2 >= 0x40 {
            (vsum_1 << 11) / vsum_2
        } else {
            1
        };

        let bias = ((409 * t) >> 15) << 15;
        let mut noise = [0i64; SUBFRAME_SIZE];
        noise[0] = clip_sinc_pcm((bias + 32358 * self.noise_state) >> 15);
        for i in 1..size {
            noise[i] = clip_sinc_pcm((bias + 32358 * noise[i - 1]) >> 15);
        }
        self.noise_state = noise[size - 1];

        for i in 0..size {
            let tmp = (self.vector_buf[i] * noise[i]) >> 11;
            // FFmpeg: repack (NCH) -> clip_work(=clip_sinc_pcm); else av_clip_int16.
            self.working_buffer[subframe_idx][i] = if self.repack {
                clip_sinc_pcm(tmp)
            } else {
                clip16(tmp)
            };
        }
    }

    fn update_state(&mut self, working_flat: &[i64]) -> Vec<i16> {
        for i in 0..6 {
            self.excitation[i] = self.excitation[288 + i];
        }
        for i in 0..288 {
            self.excitation[6 + i] = working_flat[i];
        }

        let mut output = Vec::with_capacity(OUTPUT_SAMPLES);
        let mut offset = 6usize;
        let mut a = 0usize;

        while offset < self.excitation.len() {
            let mut tmp: i64 = 0;
            for i in 0..6 {
                let idx = offset.wrapping_sub(i);
                if idx < self.excitation.len() {
                    tmp += self.excitation[idx] * SINC[a + i * 11] as i64;
                }
            }
            offset += 1;
            tmp >>= 15;
            // FFmpeg: repack (NCH) -> clip_sinc_pcm; else av_clip_int16.
            let s = if self.repack { clip_sinc_pcm(tmp) } else { clip16(tmp) };
            output.push(s as i16);

            a = (a + 1) % 11;
            if a == 0 {
                offset += 1;
            }
        }

        output.truncate(OUTPUT_SAMPLES);
        output
    }
}

/// Scale fixed-size array values by shifting
fn scale_vec(vec: &mut [i64; SUBFRAME_SIZE], bits: i32, size: usize) {
    if bits < 0 {
        let shift = (-bits) as u32;
        for v in vec[..size].iter_mut() {
            *v >>= shift;
        }
    } else if bits > 0 {
        let shift = bits as u32;
        for v in vec[..size].iter_mut() {
            *v <<= shift;
        }
    }
}

fn scale_vec_arr(vec: &mut [i64; 15], bits: i32, size: usize) {
    if bits < 0 {
        let shift = (-bits) as u32;
        for v in vec[..size].iter_mut() {
            *v >>= shift;
        }
    } else if bits > 0 {
        let shift = bits as u32;
        for v in vec[..size].iter_mut() {
            *v <<= shift;
        }
    }
}

fn vec_mult(src: &[i64; 15], mult: &[i32; 15]) -> [i64; 15] {
    let mut dst = [0i64; 15];
    dst[0] = src[0];
    for i in 1..15 {
        dst[i] = (src[i] * mult[i] as i64 + 0x4000) >> 15;
    }
    dst
}
