#!/usr/bin/env python3
"""DS2 (Olympus DSS Pro) native decoder - reverse engineered from DssDecoder.dll/DssParser.dll.

SP mode (0): 12000 Hz, mono, 16-bit, CELP codec.
QP mode (6): 16000 Hz, mono, 16-bit, CELP codec.

SP frames: 328 bits (41 bytes), MSB-first within 16-bit LE words, byte-swap demuxing.
QP frames: 448 bits (56 bytes), MSB-first within 16-bit LE words, continuous bitstream.

Synthesis uses a normalized lattice filter with reflection coefficients
(NOT standard LPC polynomial). The codebook stores quantized reflection
coefficients directly - no LSP-to-LPC conversion needed.
FUN_10019d40 in DssDecoder.dll implements this lattice filter.
"""

import math
import struct
import sys
import wave
import numpy as np
from pathlib import Path

# ==============================================================================
# SP Constants from DssDecoder.dll FUN_100180c0
# ==============================================================================

SP_SAMPLE_RATE = 12000
SP_NUM_COEFFS = 14
SP_NUM_SUBFRAMES = 4
SP_SUBFRAME_SIZE = 72
SP_SAMPLES_PER_FRAME = SP_NUM_SUBFRAMES * SP_SUBFRAME_SIZE  # 288
SP_FRAME_BITS = 328
SP_MIN_PITCH = 36
SP_MAX_PITCH = 186
SP_PITCH_RANGE = SP_MAX_PITCH - SP_MIN_PITCH + 1  # 151
SP_PITCH_DELTA_RANGE = 48
SP_EXCITATION_PULSES = 7
SP_REFL_BIT_ALLOC = [5, 5, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3]  # sum = 52
SP_PITCH_GAIN_BITS = 5
SP_GAIN_BITS = 6
SP_PULSE_BITS = 3
SP_FIXED_CB_SIZE = math.comb(SP_SUBFRAME_SIZE, SP_EXCITATION_PULSES)
SP_COMBINED_PITCH_RANGE = SP_PITCH_RANGE * (SP_PITCH_DELTA_RANGE ** (SP_NUM_SUBFRAMES - 1))
SP_COMBINED_PITCH_BITS = math.ceil(math.log2(SP_COMBINED_PITCH_RANGE))  # 24

# ==============================================================================
# QP Constants from DssDecoder.dll FUN_100179d0 + FUN_10017a80
# ==============================================================================

QP_SAMPLE_RATE = 16000
QP_NUM_COEFFS = 16
QP_NUM_SUBFRAMES = 4
QP_SUBFRAME_SIZE = 64         # 16 * 4.0
QP_SAMPLES_PER_FRAME = QP_NUM_SUBFRAMES * QP_SUBFRAME_SIZE  # 256
QP_FRAME_BITS = 448
QP_MIN_PITCH = 45
QP_MAX_PITCH = 300
QP_PITCH_RANGE = QP_MAX_PITCH - QP_MIN_PITCH + 1  # 256
QP_PITCH_DELTA_RANGE = 256
QP_EXCITATION_PULSES = 11
QP_REFL_BIT_ALLOC = [7, 7, 6, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3]  # sum = 76
QP_PITCH_GAIN_BITS = 6
QP_GAIN_BITS = 6
QP_PULSE_BITS = 3
QP_FIXED_CB_SIZE = math.comb(QP_SUBFRAME_SIZE, QP_EXCITATION_PULSES)
QP_PITCH_BITS_PER_SUBFRAME = 8  # pitch read per-subframe (ceil(log2(256))=8)

# File structure
DS2_HEADER_SIZE = 0x600
DS2_BLOCK_SIZE = 512
DS2_BLOCK_HEADER_SIZE = 6

# ==============================================================================
# SP Quantization tables from DssDecoder.dll .rdata section
# ==============================================================================

# SP Pitch prediction gain (5-bit, 32 entries at VA 0x1004CF90 + offset=32)
SP_PITCH_GAIN_TABLE = [
    0.049805, 0.112793, 0.175781, 0.238281, 0.301270, 0.364258, 0.427246, 0.490234,
    0.553223, 0.615723, 0.678711, 0.741699, 0.804688, 0.867676, 0.930176, 0.993164,
    1.056152, 1.119141, 1.182129, 1.245117, 1.307617, 1.370605, 1.433594, 1.496582,
    1.559570, 1.622559, 1.685059, 1.748047, 1.811035, 1.874023, 1.937012, 2.000000,
]

# SP Excitation gain (6-bit, 64 entries at VA 0x1004DF80 + offset=64)
SP_EXCITATION_GAIN_TABLE = [
       0,    4,    8,   13,   17,   22,   26,   31,   35,   40,
      44,   48,   53,   58,   63,   69,   76,   83,   91,   99,
     109,  119,  130,  142,  155,  170,  185,  203,  222,  242,
     265,  290,  317,  346,  378,  414,  452,  494,  540,  591,
     646,  706,  771,  843,  922, 1007, 1101, 1204, 1316, 1438,
    1572, 1719, 1879, 2053, 2244, 2453, 2682, 2931, 3204, 3502,
    3828, 4184, 4574, 5000,
]

# Pulse amplitude (3-bit, 8 entries at VA 0x1004EF70 + offset=8) — shared SP/QP
PULSE_AMP_TABLE = [
    -0.951599, -0.679718, -0.407837, -0.135956,
     0.135956,  0.407837,  0.679718,  0.951599,
]

# ==============================================================================
# QP Quantization tables from DssDecoder.dll .rdata section
# ==============================================================================

# QP Pitch prediction gain (6-bit, 64 entries at VA 0x1004BA10)
QP_PITCH_GAIN_TABLE = [
    0.004913, 0.056367, 0.102669, 0.145092, 0.184286, 0.220170, 0.252640, 0.281841,
    0.308202, 0.332237, 0.354531, 0.375491, 0.395460, 0.414675, 0.433337, 0.451622,
    0.469648, 0.487486, 0.505255, 0.523016, 0.540824, 0.558764, 0.576890, 0.595276,
    0.613963, 0.632917, 0.652245, 0.671900, 0.691902, 0.712322, 0.733015, 0.753909,
    0.774967, 0.796116, 0.817233, 0.838156, 0.858900, 0.879346, 0.899405, 0.919040,
    0.938366, 0.957462, 0.976668, 0.996526, 1.017693, 1.041066, 1.066882, 1.095219,
    1.126158, 1.159959, 1.196753, 1.236515, 1.279487, 1.325996, 1.376201, 1.429902,
    1.487140, 1.548301, 1.613491, 1.682657, 1.755914, 1.833605, 1.914886, 1.999406,
]

# QP Excitation gain (6-bit, 64 entries at VA 0x1004BC10)
QP_EXCITATION_GAIN_TABLE = [
      3.928,   7.069,  10.993,  16.465,  23.856,  32.753,  42.893,  54.076,
     66.160,  79.016,  92.493, 106.558, 121.106, 136.128, 151.663, 167.700,
    184.251, 201.424, 219.212, 237.740, 257.014, 277.134, 298.164, 320.054,
    342.913, 366.849, 391.851, 418.102, 445.564, 474.334, 504.476, 536.280,
    569.771, 604.926, 642.050, 681.112, 722.397, 766.071, 812.234, 861.189,
    913.161, 968.356, 1027.220, 1089.687, 1156.595, 1228.228, 1305.279, 1387.811,
    1476.597, 1572.636, 1675.856, 1789.017, 1911.832, 2045.863, 2194.195, 2360.133,
    2545.084, 2752.592, 2991.921, 3271.340, 3603.855, 4004.808, 4476.587, 4970.296,
]

# QP Pulse amplitude (3-bit, 8 entries at VA 0x1004BE10) — different from SP
QP_PULSE_AMP_TABLE = [
    -0.921705, -0.628998, -0.397315, -0.140886,
     0.206959,  0.433678,  0.652927,  0.931249,
]


# ==============================================================================
# Bitstream readers
# ==============================================================================

class SPBitstreamReader:
    """SP mode: MSB-first within 16-bit LE words (FUN_10017460)."""

    def __init__(self, data):
        self.data = data
        self.word_index = 0
        self.mask = 0
        self.current_word = 0

    def _load_next_word(self):
        offset = self.word_index * 2
        if offset + 1 < len(self.data):
            self.current_word = self.data[offset] | (self.data[offset + 1] << 8)
        else:
            self.current_word = 0
        self.word_index += 1

    def read_bits(self, n):
        if n <= 0:
            return 0
        result = 0
        result_mask = 1 << (n - 1)
        for _ in range(n):
            if self.mask == 0:
                self.mask = 0x8000
                self._load_next_word()
            else:
                self.mask >>= 1
                if self.mask == 0:
                    self.mask = 0x8000
                    self._load_next_word()
            if self.current_word & self.mask:
                result |= result_mask
            result_mask >>= 1
        return result



# ==============================================================================
# DS2 file reader
# ==============================================================================

DSS_SP_PACKET_SIZE = 42

def read_ds2_file(path, password=None):
    """Read DS2 file, detect mode (SP or QP), extract frame data.

    SP mode (0-1): byte-swap demuxing, returns list of 42-byte packets.
    QP mode (6-7): continuous bitstream, returns raw byte stream + frame count.

    If the file is encrypted (magic \\x03enc), password is required.

    Returns: (frame_data, total_frames, mode)
      mode 'sp': frame_data is list of 42-byte packets
      mode 'qp': frame_data is a single bytes object (continuous bitstream)
    """
    with open(path, 'rb') as f:
        data = f.read()

    if data[:4] == b'\x03enc':
        if not password:
            raise ValueError(
                f"Encrypted DS2 file requires a password: {path} "
                "(use --password or decrypt with ds2decrypt.py first)"
            )
        from ds2decrypt import decrypt_encrypted_ds2

        data = decrypt_encrypted_ds2(data, password)
    elif data[:4] != b'\x03ds2':
        raise ValueError(f"Not a DS2 file: {path}")

    num_blocks = (len(data) - DS2_HEADER_SIZE) // DS2_BLOCK_SIZE

    # Detect format from first block header byte 4
    format_type = data[DS2_HEADER_SIZE + 4]

    total_frames = 0
    for bi in range(num_blocks):
        total_frames += data[DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE + 2]

    if format_type >= 6:
        # QP mode: continuous bitstream (no byte-swap)
        stream = bytearray()
        for bi in range(num_blocks):
            bstart = DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE
            stream.extend(data[bstart + DS2_BLOCK_HEADER_SIZE:bstart + DS2_BLOCK_SIZE])
        return bytes(stream), total_frames, 'qp'
    else:
        # SP mode: byte-swap demuxing
        stream = bytearray()
        for bi in range(num_blocks):
            bstart = DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE
            stream.extend(data[bstart + DS2_BLOCK_HEADER_SIZE:bstart + DS2_BLOCK_SIZE])

        swap = (data[DS2_HEADER_SIZE] >> 7) & 1
        swap_byte = 0
        pos = 0
        frame_packets = []

        for fi in range(total_frames):
            pkt = bytearray(DSS_SP_PACKET_SIZE + 1)
            if swap:
                read_size = 40
                end = min(pos + read_size, len(stream))
                pkt[3:3 + (end - pos)] = stream[pos:end]
                pos += read_size
                for i in range(0, DSS_SP_PACKET_SIZE - 2, 2):
                    pkt[i] = pkt[i + 4]
                pkt[DSS_SP_PACKET_SIZE] = 0
                pkt[1] = swap_byte
            else:
                read_size = DSS_SP_PACKET_SIZE
                end = min(pos + read_size, len(stream))
                pkt[:end - pos] = stream[pos:end]
                pos += read_size
                swap_byte = pkt[DSS_SP_PACKET_SIZE - 2]
            pkt[DSS_SP_PACKET_SIZE - 2] = 0
            swap ^= 1
            frame_packets.append(bytes(pkt[:DSS_SP_PACKET_SIZE]))

        return frame_packets, total_frames, 'sp'


# ==============================================================================
# Codebook loading
# ==============================================================================

def load_codebook(path, num_coeffs):
    """Load per-coefficient codebook vectors."""
    data = np.load(path)
    codebook = [data[f'coeff_{i}'] for i in range(num_coeffs)]

    # SP coeff_13 fix: only 4 entries extracted from DLL, but needs 8 (3-bit index)
    if num_coeffs == 14 and len(codebook[13]) < 8:
        ffmpeg_row13 = np.array([-11239, -7220, -4040, -1406, 971, 3321, 6006, 9697],
                                dtype=np.float64) / 32768
        codebook[13] = ffmpeg_row13

    return codebook


def dequantize_reflection_coeffs(indices, codebook, num_coeffs):
    """Look up reflection coefficients from codebook indices."""
    coeffs = np.zeros(num_coeffs)
    for i in range(num_coeffs):
        idx = indices[i]
        cb = codebook[i]
        coeffs[i] = cb[min(idx, len(cb) - 1)]
    return coeffs


# ==============================================================================
# Normalized lattice synthesis filter (FUN_10019d40)
# ==============================================================================

def lattice_synthesis(excitation, coeffs, state):
    """Normalized lattice synthesis filter matching DssDecoder.dll FUN_10019d40."""
    P = len(coeffs)
    temp = state.copy()
    output = np.zeros(len(excitation))

    for n in range(len(excitation)):
        acc = excitation[n] - temp[P - 1] * coeffs[P - 1]
        for k in range(P - 2, -1, -1):
            acc -= temp[k] * coeffs[k]
            temp[k + 1] = coeffs[k] * acc + temp[k]
        temp[0] = acc
        output[n] = acc

    return output, temp.copy()


# ==============================================================================
# Pitch decoding
# ==============================================================================

def decode_combined_pitch(combined, pitch_range, min_pitch, delta_range, num_subframes):
    """Decode combined pitch value to per-subframe pitch lags."""
    p0_idx = combined % pitch_range
    remaining = combined // pitch_range

    deltas = []
    for i in range(num_subframes - 2):
        deltas.append(remaining % delta_range)
        remaining //= delta_range
    deltas.append(min(remaining, delta_range - 1))

    pitches = [p0_idx + min_pitch]
    for delta_idx in deltas:
        prev = pitches[-1]
        # Delta base calculation matching FFmpeg/DLL:
        # base = max(min_pitch, prev - half_delta)
        # clamped so base + delta_range - 1 <= max_pitch
        half_delta = delta_range // 2 - 1
        upper_limit = min_pitch + pitch_range - 1 - half_delta
        if prev > upper_limit:
            base = upper_limit - half_delta
        else:
            base = max(min_pitch, prev - half_delta)
        pitches.append(base + delta_idx)

    return pitches


# ==============================================================================
# Fixed codebook - combinatorial number system
# ==============================================================================

def decode_combinatorial_index(index, n, k):
    """Decode combinatorial number system index to k positions from {0..n-1}."""
    positions = []
    remaining = index
    for i in range(k, 0, -1):
        v = i - 1
        while v + 1 < n and math.comb(v + 1, i) <= remaining:
            v += 1
        positions.append(v)
        remaining -= math.comb(v, i)
    return positions


# ==============================================================================
# DS2 Decoder
# ==============================================================================

class DS2Decoder:
    def __init__(self, mode='sp'):
        self.mode = mode
        if mode == 'sp':
            self.sample_rate = SP_SAMPLE_RATE
            self.num_coeffs = SP_NUM_COEFFS
            self.num_subframes = SP_NUM_SUBFRAMES
            self.subframe_size = SP_SUBFRAME_SIZE
            self.samples_per_frame = SP_SAMPLES_PER_FRAME
            self.min_pitch = SP_MIN_PITCH
            self.max_pitch = SP_MAX_PITCH
            self.pitch_range = SP_PITCH_RANGE
            self.pitch_delta_range = SP_PITCH_DELTA_RANGE
            self.excitation_pulses = SP_EXCITATION_PULSES
            self.refl_bit_alloc = SP_REFL_BIT_ALLOC
            self.pitch_gain_bits = SP_PITCH_GAIN_BITS
            self.gain_bits = SP_GAIN_BITS
            self.pulse_bits = SP_PULSE_BITS
            self.combined_pitch_bits = SP_COMBINED_PITCH_BITS
            self.pitch_gain_table = SP_PITCH_GAIN_TABLE
            self.excitation_gain_table = SP_EXCITATION_GAIN_TABLE
            self.frame_bits = SP_FRAME_BITS
            self.codebook = load_codebook('ds2_lsp_codebook.npz', SP_NUM_COEFFS)
        else:
            self.sample_rate = QP_SAMPLE_RATE
            self.num_coeffs = QP_NUM_COEFFS
            self.num_subframes = QP_NUM_SUBFRAMES
            self.subframe_size = QP_SUBFRAME_SIZE
            self.samples_per_frame = QP_SAMPLES_PER_FRAME
            self.min_pitch = QP_MIN_PITCH
            self.max_pitch = QP_MAX_PITCH
            self.pitch_range = QP_PITCH_RANGE
            self.pitch_delta_range = QP_PITCH_DELTA_RANGE
            self.excitation_pulses = QP_EXCITATION_PULSES
            self.refl_bit_alloc = QP_REFL_BIT_ALLOC
            self.pitch_gain_bits = QP_PITCH_GAIN_BITS
            self.gain_bits = QP_GAIN_BITS
            self.pulse_bits = QP_PULSE_BITS
            self.pitch_bits_per_subframe = QP_PITCH_BITS_PER_SUBFRAME
            self.pitch_gain_table = QP_PITCH_GAIN_TABLE
            self.excitation_gain_table = QP_EXCITATION_GAIN_TABLE
            self.frame_bits = QP_FRAME_BITS
            self.codebook = load_codebook('ds2_qp_codebook.npz', QP_NUM_COEFFS)
            self.pulse_amp_table = QP_PULSE_AMP_TABLE

        if mode == 'sp':
            self.pulse_amp_table = PULSE_AMP_TABLE
        self.lattice_state = np.zeros(self.num_coeffs)
        self.pitch_memory = np.zeros(self.max_pitch + self.subframe_size)
        self.prng_state = 0

    def decode_file(self, ds2_path, wav_path=None, password=None):
        frame_data, total_frames, detected_mode = read_ds2_file(ds2_path, password=password)

        if detected_mode != self.mode:
            print(f"Warning: file is {detected_mode} but decoder is {self.mode}, switching")
            self.__init__(detected_mode)
            frame_data, total_frames, detected_mode = read_ds2_file(ds2_path, password=password)

        all_samples = []

        if self.mode == 'qp':
            all_samples = self._decode_qp_frames(frame_data, total_frames)
        else:
            all_samples = self._decode_sp_frames(frame_data, total_frames)

        duration = len(all_samples) / self.sample_rate
        print(f"Decoded: {total_frames} frames, {duration:.2f}s at {self.sample_rate}Hz")

        samples_arr = np.array(all_samples, dtype=np.float64)

        # QP mode: apply de-emphasis filter y[n] = x[n] + alpha*y[n-1]
        # Matches DssDecoder.dll FUN_10018ca0 with DAT_10066988 = 0.1
        if self.mode == 'qp':
            alpha = 0.1
            for i in range(1, len(samples_arr)):
                samples_arr[i] += alpha * samples_arr[i - 1]

        # Convert to int16 via truncation (matching DLL's cvttsd2si)
        samples_16 = np.clip(samples_arr, -32768, 32767).astype(np.int16)

        if wav_path:
            with wave.open(wav_path, 'w') as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(self.sample_rate)
                w.writeframes(samples_16.tobytes())
            print(f"Written: {wav_path}")

        return samples_16

    def _decode_sp_frames(self, frame_packets, total_frames):
        """Decode SP frames from byte-swap demuxed packets."""
        all_samples = []
        for fi in range(total_frames):
            reader = SPBitstreamReader(frame_packets[fi])
            refl_indices = [reader.read_bits(b) for b in self.refl_bit_alloc]

            subframe_data = []
            for sf in range(self.num_subframes):
                pg_idx = reader.read_bits(self.pitch_gain_bits)
                cb_idx = reader.read_bits(math.ceil(math.log2(
                    math.comb(self.subframe_size, self.excitation_pulses))))
                gain_idx = reader.read_bits(self.gain_bits)
                pulses = [reader.read_bits(self.pulse_bits)
                          for _ in range(self.excitation_pulses)]
                subframe_data.append({
                    'pitch_gain_idx': pg_idx,
                    'cb_index': cb_idx,
                    'gain_idx': gain_idx,
                    'pulses': pulses,
                })

            combined_pitch = reader.read_bits(self.combined_pitch_bits)
            samples = self._decode_speech(refl_indices, subframe_data, combined_pitch)
            all_samples.extend(samples)
        return all_samples

    def _decode_qp_frames(self, stream, total_frames):
        """Decode QP frames from continuous bitstream.

        QP uses the same MSB-first 16-bit LE word bitstream reader as SP
        (both call FUN_10017460 in DssDecoder.dll).

        QP frame structure (448 bits):
          - Reflection coefficients: 76 bits [7,7,6,6,5,5,5,5,5,4,4,4,4,3,3,3]
          - Per subframe x4: pitch(8) + pitch_gain(6) + cb_index(40) + exc_gain(6)
                           + pulses(3x11=33) = 93 bits
          - Total: 76 + 4*93 = 448
        """
        all_samples = []
        reader = SPBitstreamReader(stream)
        cb_bits = math.ceil(math.log2(
            math.comb(self.subframe_size, self.excitation_pulses)))

        for fi in range(total_frames):
            refl_indices = [reader.read_bits(b) for b in self.refl_bit_alloc]

            subframe_data = []
            pitches = []
            for sf in range(self.num_subframes):
                pitch_idx = reader.read_bits(8)
                pg_idx = reader.read_bits(self.pitch_gain_bits)
                cb_idx = reader.read_bits(cb_bits)
                gain_idx = reader.read_bits(self.gain_bits)
                pulses = [reader.read_bits(self.pulse_bits)
                          for _ in range(self.excitation_pulses)]
                pitches.append(pitch_idx + self.min_pitch)
                subframe_data.append({
                    'pitch_gain_idx': pg_idx,
                    'cb_index': cb_idx,
                    'gain_idx': gain_idx,
                    'pulses': pulses,
                })

            samples = self._decode_speech_with_pitches(
                refl_indices, subframe_data, pitches)
            all_samples.extend(samples)
        return all_samples

    def _decode_speech(self, refl_indices, subframe_data, combined_pitch):
        """Decode one SP speech frame (combined pitch at end of frame)."""
        pitches = decode_combined_pitch(
            combined_pitch, self.pitch_range, self.min_pitch,
            self.pitch_delta_range, self.num_subframes)
        return self._decode_speech_with_pitches(refl_indices, subframe_data, pitches)

    def _decode_speech_with_pitches(self, refl_indices, subframe_data, pitches):
        """Decode one speech frame given pre-decoded pitch lags."""
        coeffs = dequantize_reflection_coeffs(refl_indices, self.codebook, self.num_coeffs)

        all_output = []
        for sf in range(self.num_subframes):
            sd = subframe_data[sf]
            pitch = pitches[sf]

            gp = self.pitch_gain_table[sd['pitch_gain_idx']]

            adaptive_exc = np.zeros(self.subframe_size)
            for i in range(self.subframe_size):
                if pitch < self.subframe_size:
                    mem_idx = len(self.pitch_memory) - pitch + (i % pitch)
                else:
                    mem_idx = len(self.pitch_memory) - pitch + i
                if 0 <= mem_idx < len(self.pitch_memory):
                    adaptive_exc[i] = self.pitch_memory[mem_idx]

            gc = float(self.excitation_gain_table[sd['gain_idx']])

            positions = decode_combinatorial_index(
                sd['cb_index'], self.subframe_size, self.excitation_pulses)
            fixed_exc = np.zeros(self.subframe_size)
            for pi, pos in enumerate(positions):
                if pos < self.subframe_size:
                    amp = self.pulse_amp_table[sd['pulses'][pi]] * gc
                    fixed_exc[pos] += amp

            excitation = gp * adaptive_exc + fixed_exc

            output, self.lattice_state = lattice_synthesis(
                excitation, coeffs, self.lattice_state)

            self.pitch_memory = np.roll(self.pitch_memory, -self.subframe_size)
            self.pitch_memory[-self.subframe_size:] = excitation

            all_output.extend(output)

        return all_output


# ==============================================================================
# CLI
# ==============================================================================

if __name__ == '__main__':
    import argparse

    parser = argparse.ArgumentParser(description="Decode Olympus DSS/DS2 audio to WAV")
    parser.add_argument("input", help="Input .ds2 or .dss file")
    parser.add_argument("output", nargs="?", help="Output .wav path (default: input.decoded.wav)")
    parser.add_argument(
        "-p", "--password",
        help="Password for encrypted DS2 files (magic \\x03enc)",
    )
    args = parser.parse_args()

    ds2_path = args.input
    wav_path = args.output or str(Path(ds2_path).with_suffix(".decoded.wav"))

    decoder = DS2Decoder()
    samples = decoder.decode_file(ds2_path, wav_path, password=args.password)
