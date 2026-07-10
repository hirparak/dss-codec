//! DS2 QP decoder - f64 lattice synthesis, 16000 Hz output.
//!
//! Supports both classic QP (format 6, 56-byte continuous frames) and
//! format-7/QP7 records with a leading mode bit and 12-byte short records.

use crate::bitstream::BitstreamReader;
use crate::codec::common::{decode_combinatorial_index, lattice_synthesis};
use crate::demux::ds2::{Ds2Qp7Segment, Ds2QpSegment};
use crate::tables::ds2_qp::qp_codebook_lookup;
use crate::tables::ds2_quant::{QP_EXCITATION_GAIN, QP_PITCH_GAIN, QP_PULSE_AMP};

const NUM_COEFFS: usize = 16;
const NUM_SUBFRAMES: usize = 4;
const SUBFRAME_SIZE: usize = 64;
const SAMPLES_PER_FRAME: usize = NUM_SUBFRAMES * SUBFRAME_SIZE;
const MIN_PITCH: u32 = 45;
const MAX_PITCH: u32 = 300;
const EXCITATION_PULSES: usize = 11;
const REFL_BIT_ALLOC: [u32; 16] = [7, 7, 6, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3];
const QP7_REFL_BIT_ALLOC: [u32; 16] = [7, 7, 6, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 2];
const PITCH_GAIN_BITS: u32 = 6;
const GAIN_BITS: u32 = 6;
const PULSE_BITS: u32 = 3;
const PITCH_BITS: u32 = 8;
const CB_BITS: u32 = 40;
const QP7_SHORT_GAIN_BITS: u32 = 5;
const QP7_SHORT_GAIN_TABLE: [f64; 64] = [
    0.0, 0.000152587890625, 0.00030517578125, 0.000457763671875,
    0.000640869140625, 0.0008544921875, 0.001068115234375, 0.00128173828125,
    0.00152587890625, 0.001800537109375, 0.002105712890625, 0.002410888671875,
    0.00274658203125, 0.00311279296875, 0.003509521484375, 0.003936767578125,
    0.00439453125, 0.0048828125, 0.00543212890625, 0.006011962890625,
    0.00665283203125, 0.00732421875, 0.008056640625, 0.00885009765625,
    0.00970458984375, 0.0106201171875, 0.011627197265625, 0.012725830078125,
    0.013885498046875, 0.01513671875, 0.016510009765625, 0.017974853515625,
    0.0498046875, 0.11279296875, 0.17578125, 0.23828125, 0.30126953125,
    0.3642578125, 0.42724609375, 0.490234375, 0.55322265625, 0.61572265625,
    0.6787109375, 0.74169921875, 0.8046875, 0.86767578125, 0.93017578125,
    0.9931640625, 1.05615234375, 1.119140625, 1.18212890625, 1.2451171875,
    1.3076171875, 1.37060546875, 1.43359375, 1.49658203125, 1.5595703125,
    1.62255859375, 1.68505859375, 1.748046875, 1.81103515625, 1.8740234375,
    1.93701171875, 2.0,
];

pub struct Ds2QpDecoder {
    lattice_state: [f64; NUM_COEFFS],
    pitch_memory: Vec<f64>,
    deemph_state: f64,
    prng_state: u16,
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
            prng_state: 0,
        }
    }

    pub fn reset_state(&mut self) {
        self.lattice_state = [0.0; NUM_COEFFS];
        self.pitch_memory.fill(0.0);
        self.deemph_state = 0.0;
        self.prng_state = 0;
    }

    pub fn decode_all_frames(&mut self, stream: &[u8], total_frames: usize) -> Vec<f64> {
        let mut reader = BitstreamReader::new(stream);
        let mut all_samples = Vec::with_capacity(total_frames * SAMPLES_PER_FRAME);

        for _ in 0..total_frames {
            all_samples.extend_from_slice(&self.decode_frame_from_reader(&mut reader));
        }

        self.apply_deemphasis(&mut all_samples);
        all_samples
    }

    pub fn decode_frame(&mut self, frame_bytes: &[u8]) -> Vec<f64> {
        let mut reader = BitstreamReader::new(frame_bytes);
        let mut samples = self.decode_frame_from_reader(&mut reader);
        self.apply_deemphasis(&mut samples);
        samples
    }

    pub fn decode_qp_segments(&mut self, segments: &[Ds2QpSegment]) -> Vec<f64> {
        let mut all = Vec::new();
        for seg in segments {
            if seg.reset_before {
                self.reset_state();
            }
            let mut seg_samples = self.decode_qp_segment(&seg.stream, seg.total_frames);
            self.apply_deemphasis(&mut seg_samples);
            all.extend(seg_samples);
        }
        all
    }

    pub fn decode_qp7_segments(&mut self, segments: &[Ds2Qp7Segment]) -> Vec<f64> {
        let mut all = Vec::new();
        for seg in segments {
            if seg.reset_before {
                self.reset_state();
            }
            let mut seg_samples = self.decode_qp7_records(&seg.records);
            self.apply_deemphasis(&mut seg_samples);
            all.extend(seg_samples);
        }
        all
    }

    fn decode_qp_segment(&mut self, stream: &[u8], total_frames: usize) -> Vec<f64> {
        let mut reader = BitstreamReader::new(stream);
        let mut all_samples = Vec::with_capacity(total_frames * SAMPLES_PER_FRAME);
        for _ in 0..total_frames {
            all_samples.extend_from_slice(&self.decode_frame_from_reader(&mut reader));
        }
        all_samples
    }

    fn decode_qp7_records(&mut self, records: &[Vec<u8>]) -> Vec<f64> {
        let mut all_samples = Vec::with_capacity(records.len() * SAMPLES_PER_FRAME);

        for record in records {
            let mut reader = BitstreamReader::new(record);
            let is_short = reader.read_bits(1) == 0;

            let mut refl_indices = [0usize; NUM_COEFFS];
            for i in 0..NUM_COEFFS {
                refl_indices[i] = reader.read_bits(QP7_REFL_BIT_ALLOC[i]) as usize;
            }

            if is_short {
                let coeffs = self.dequantize_reflection_coeffs(&refl_indices);
                for _ in 0..NUM_SUBFRAMES {
                    let gain_idx = reader.read_bits(QP7_SHORT_GAIN_BITS) as usize;
                    let gain = QP7_SHORT_GAIN_TABLE[gain_idx.min(QP7_SHORT_GAIN_TABLE.len() - 1)];
                    let mut excitation = [0.0f64; SUBFRAME_SIZE];
                    for sample in &mut excitation {
                        self.prng_state = self.prng_state.wrapping_mul(0x209).wrapping_add(0x103);
                        *sample = (self.prng_state as i16) as f64 * gain;
                    }
                    let output = lattice_synthesis(&excitation, &coeffs, &mut self.lattice_state);
                    self.update_pitch_memory(&excitation);
                    all_samples.extend_from_slice(&output);
                }
                continue;
            }

            all_samples.extend_from_slice(&self.decode_qp7_long_record(&mut reader, &refl_indices));
        }

        all_samples
    }

    fn decode_frame_from_reader(&mut self, reader: &mut BitstreamReader) -> Vec<f64> {
        let mut refl_indices = [0usize; NUM_COEFFS];
        for i in 0..NUM_COEFFS {
            refl_indices[i] = reader.read_bits(REFL_BIT_ALLOC[i]) as usize;
        }

        self.decode_long_like_record(reader, &refl_indices)
    }

    fn decode_qp7_long_record(
        &mut self,
        reader: &mut BitstreamReader,
        refl_indices: &[usize; NUM_COEFFS],
    ) -> Vec<f64> {
        self.decode_long_like_record_with_alloc(reader, refl_indices, PITCH_BITS)
    }

    fn decode_long_like_record(
        &mut self,
        reader: &mut BitstreamReader,
        refl_indices: &[usize; NUM_COEFFS],
    ) -> Vec<f64> {
        self.decode_long_like_record_with_alloc(reader, refl_indices, PITCH_BITS)
    }

    fn decode_long_like_record_with_alloc(
        &mut self,
        reader: &mut BitstreamReader,
        refl_indices: &[usize; NUM_COEFFS],
        pitch_bits: u32,
    ) -> Vec<f64> {
        let mut subframe_data = Vec::with_capacity(NUM_SUBFRAMES);
        let mut pitches = Vec::with_capacity(NUM_SUBFRAMES);

        for _ in 0..NUM_SUBFRAMES {
            let pitch_idx = reader.read_bits(pitch_bits);
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

        let coeffs = self.dequantize_reflection_coeffs(refl_indices);
        self.decode_subframes_with_coeffs(&coeffs, &subframe_data, &pitches)
    }

    fn dequantize_reflection_coeffs(&self, refl_indices: &[usize; NUM_COEFFS]) -> [f64; NUM_COEFFS] {
        let mut coeffs = [0.0f64; NUM_COEFFS];
        for i in 0..NUM_COEFFS {
            coeffs[i] = qp_codebook_lookup(i, refl_indices[i]);
        }
        coeffs
    }

    fn decode_subframes_with_coeffs(
        &mut self,
        coeffs: &[f64; NUM_COEFFS],
        subframe_data: &[(usize, u64, usize, [usize; EXCITATION_PULSES])],
        pitches: &[u32],
    ) -> Vec<f64> {
        let mut all_output = Vec::with_capacity(SAMPLES_PER_FRAME);

        for sf in 0..NUM_SUBFRAMES {
            let (pg_idx, cb_idx, gain_idx, pulses) = &subframe_data[sf];
            let pitch = pitches[sf] as usize;
            let gp = QP_PITCH_GAIN[*pg_idx];

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

            let gc = QP_EXCITATION_GAIN[*gain_idx];
            let positions = decode_combinatorial_index(*cb_idx, SUBFRAME_SIZE, EXCITATION_PULSES);
            let mut fixed_exc = [0.0f64; SUBFRAME_SIZE];
            for (pi, &pos) in positions.iter().enumerate() {
                if pos < SUBFRAME_SIZE {
                    fixed_exc[pos] += QP_PULSE_AMP[pulses[pi]] * gc;
                }
            }

            let mut excitation = [0.0f64; SUBFRAME_SIZE];
            for i in 0..SUBFRAME_SIZE {
                excitation[i] = gp * adaptive_exc[i] + fixed_exc[i];
            }

            let output = lattice_synthesis(&excitation, coeffs, &mut self.lattice_state);
            self.update_pitch_memory(&excitation);
            all_output.extend_from_slice(&output);
        }

        all_output
    }

    fn update_pitch_memory(&mut self, excitation: &[f64; SUBFRAME_SIZE]) {
        let mem_len = self.pitch_memory.len();
        self.pitch_memory.copy_within(SUBFRAME_SIZE..mem_len, 0);
        let start = mem_len - SUBFRAME_SIZE;
        self.pitch_memory[start..].copy_from_slice(excitation);
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
