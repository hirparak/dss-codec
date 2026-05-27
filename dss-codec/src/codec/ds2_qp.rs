//! DS2 QP decoder — f64 lattice synthesis, 16000 Hz output.
//!
//! 16 reflection coefficients, 64-sample subframes, 4 subframes/frame,
//! 11-pulse combinatorial codebook C(64,11), continuous bitstream,
//! per-subframe pitch encoding (8 bits each), de-emphasis filter.

use crate::bitstream::BitstreamReader;
use crate::codec::common::{decode_combinatorial_index, lattice_synthesis};
use crate::tables::ds2_qp::qp_codebook_lookup;
use crate::tables::ds2_quant::{QP_EXCITATION_GAIN, QP_PITCH_GAIN, QP_PULSE_AMP};

const NUM_COEFFS: usize = 16;
const NUM_SUBFRAMES: usize = 4;
const SUBFRAME_SIZE: usize = 64;
const SAMPLES_PER_FRAME: usize = NUM_SUBFRAMES * SUBFRAME_SIZE; // 256
const MIN_PITCH: u32 = 45;
const MAX_PITCH: u32 = 300;
const EXCITATION_PULSES: usize = 11;
const REFL_BIT_ALLOC: [u32; 16] = [7, 7, 6, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3];
const PITCH_GAIN_BITS: u32 = 6;
const GAIN_BITS: u32 = 6;
const PULSE_BITS: u32 = 3;
const PITCH_BITS: u32 = 8;
// CB_BITS = ceil(log2(C(64,11))) = ceil(log2(7669339132720)) = 43... actually 40
// C(64,11) = 7669339132720, log2 ~ 42.8 => 43 bits? No, Python says 40.
// math.ceil(math.log2(math.comb(64,11))) = 43. Let me recalculate.
// 2^42 = 4398046511104, 2^43 = 8796093022208
// C(64,11) = 7669339132720 < 8796093022208 = 2^43, so 43 bits? No.
// Actually from Python: math.ceil(math.log2(math.comb(64,11))) = 43
// But the frame is 448 bits total. Let's verify:
// 76 (refl) + 4*(8+6+CB+6+33) = 76 + 4*(53+CB) = 448
// 448 - 76 = 372, 372/4 = 93, 93 - 53 = 40. So CB_BITS = 40.
const CB_BITS: u32 = 40;

pub struct Ds2QpDecoder {
    lattice_state: [f64; NUM_COEFFS],
    pitch_memory: Vec<f64>,
    deemph_state: f64,
}

impl Default for Ds2QpDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Ds2QpDecoder {
    pub fn new() -> Self {
        Self {
            lattice_state: [0.0; NUM_COEFFS],
            pitch_memory: vec![0.0; MAX_PITCH as usize + SUBFRAME_SIZE],
            deemph_state: 0.0,
        }
    }

    /// Decode all QP frames from a continuous bitstream. Returns all samples as f64.
    /// De-emphasis is applied at the end.
    pub fn decode_all_frames(&mut self, stream: &[u8], total_frames: usize) -> Vec<f64> {
        let mut reader = BitstreamReader::new(stream);
        let mut all_samples = Vec::with_capacity(total_frames * SAMPLES_PER_FRAME);

        for _ in 0..total_frames {
            let mut frame = [0u8; 56];
            for word in 0..28 {
                let value = reader.read_bits(16) as u16;
                frame[word * 2] = value as u8;
                frame[word * 2 + 1] = (value >> 8) as u8;
            }
            let samples = self.decode_frame(&frame);
            all_samples.extend_from_slice(&samples);
        }

        all_samples
    }

    /// Decode a single QP frame from its 56-byte payload.
    pub fn decode_frame(&mut self, frame_bytes: &[u8]) -> Vec<f64> {
        let mut reader = BitstreamReader::new(frame_bytes);
        let mut samples = self.decode_frame_from_reader(&mut reader);
        self.apply_deemphasis(&mut samples);
        samples
    }

    fn decode_frame_from_reader(&mut self, reader: &mut BitstreamReader) -> Vec<f64> {
        // Read reflection coefficient indices
        let mut refl_indices = [0usize; NUM_COEFFS];
        for i in 0..NUM_COEFFS {
            refl_indices[i] = reader.read_bits(REFL_BIT_ALLOC[i]) as usize;
        }

        // Read per-subframe parameters (pitch is per-subframe, not combined)
        let mut subframe_data = Vec::with_capacity(NUM_SUBFRAMES);
        let mut pitches = Vec::with_capacity(NUM_SUBFRAMES);

        for _ in 0..NUM_SUBFRAMES {
            let pitch_idx = reader.read_bits(PITCH_BITS);
            let pg_idx = reader.read_bits(PITCH_GAIN_BITS) as usize;
            let cb_idx = reader.read_bits_u64(CB_BITS);
            let gain_idx = reader.read_bits(GAIN_BITS) as usize;
            let mut pulses = [0usize; EXCITATION_PULSES];
            for p in &mut pulses {
                *p = reader.read_bits(PULSE_BITS) as usize;
            }
            pitches.push(pitch_idx + MIN_PITCH);
            subframe_data.push((pg_idx, cb_idx, gain_idx, pulses));
        }

        // Dequantize reflection coefficients
        let mut coeffs = [0.0f64; NUM_COEFFS];
        for i in 0..NUM_COEFFS {
            coeffs[i] = qp_codebook_lookup(i, refl_indices[i]);
        }

        // Decode subframes
        let mut all_output = Vec::with_capacity(SAMPLES_PER_FRAME);

        for sf in 0..NUM_SUBFRAMES {
            let (pg_idx, cb_idx, gain_idx, pulses) = &subframe_data[sf];
            let pitch = pitches[sf] as usize;
            let gp = QP_PITCH_GAIN[*pg_idx];

            // Adaptive excitation from pitch memory
            let mut adaptive_exc = [0.0f64; SUBFRAME_SIZE];
            let mem_len = self.pitch_memory.len();
            for i in 0..SUBFRAME_SIZE {
                let mem_idx = if pitch < SUBFRAME_SIZE {
                    mem_len - pitch + (i % pitch)
                } else {
                    mem_len - pitch + i
                };
                if mem_idx < mem_len {
                    adaptive_exc[i] = self.pitch_memory[mem_idx];
                }
            }

            // Fixed codebook excitation
            let gc = QP_EXCITATION_GAIN[*gain_idx];
            let positions = decode_combinatorial_index(*cb_idx, SUBFRAME_SIZE, EXCITATION_PULSES);
            let mut fixed_exc = [0.0f64; SUBFRAME_SIZE];
            for (pi, &pos) in positions.iter().enumerate() {
                if pos < SUBFRAME_SIZE {
                    fixed_exc[pos] += QP_PULSE_AMP[pulses[pi]] * gc;
                }
            }

            // Total excitation
            let mut excitation = [0.0f64; SUBFRAME_SIZE];
            for i in 0..SUBFRAME_SIZE {
                excitation[i] = gp * adaptive_exc[i] + fixed_exc[i];
            }

            // Lattice synthesis
            let output = lattice_synthesis(&excitation, &coeffs, &mut self.lattice_state);

            // Update pitch memory
            let mem_len = self.pitch_memory.len();
            self.pitch_memory.copy_within(SUBFRAME_SIZE..mem_len, 0);
            let start = mem_len - SUBFRAME_SIZE;
            self.pitch_memory[start..].copy_from_slice(&excitation);

            all_output.extend_from_slice(&output);
        }

        all_output
    }

    fn apply_deemphasis(&mut self, samples: &mut [f64]) {
        let alpha = 0.1;
        if !samples.is_empty() {
            samples[0] += alpha * self.deemph_state;
            for i in 1..samples.len() {
                samples[i] += alpha * samples[i - 1];
            }
            self.deemph_state = *samples.last().unwrap();
        }
    }
}
