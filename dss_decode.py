#!/usr/bin/env python3
"""DSS (Digital Speech Standard) SP mode decoder.

Ported from FFmpeg's libavcodec/dss_sp.c (LGPL 2.1+).
Original C code: Copyright (C) 2014 Oleksij Rempel <linux@rempel-privat.de>

The DSS SP codec uses a CELP architecture with:
- 14 reflection coefficients converted to LPC polynomial
- Pitch-adaptive excitation from history buffer
- Fixed codebook (7 pulses, combinatorial encoding)
- Cascaded LPC synthesis + error correction filters
- Noise modulation for comfort noise
- 11:12 sinc interpolation resampling (12000 -> 11025 Hz)

Internal processing: 72 samples/subframe, 4 subframes = 288 samples at 12000 Hz
Output: 264 samples/frame at 11025 Hz (sinc-interpolated)
"""

import math
import struct
import sys
import wave
import numpy as np
from pathlib import Path

# ==============================================================================
# Constants
# ==============================================================================

DSS_SP_FRAME_SIZE = 42       # bytes per frame packet
DSS_SP_SUBFRAMES = 4
DSS_SP_SUBFRAME_SIZE = 72    # internal subframe size
DSS_SP_SAMPLE_COUNT = 66 * DSS_SP_SUBFRAMES  # 264 output samples per frame
DSS_SP_SAMPLE_RATE = 11025

DSS_BLOCK_SIZE = 512
DSS_BLOCK_HEADER_SIZE = 6

# ==============================================================================
# Tables from FFmpeg dss_sp.c
# ==============================================================================

# Combinatorial table for pulse position decoding [8][72]
COMBINATORIAL_TABLE = [
    [0]*72,
    list(range(72)),
    [0, 0, 1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 66, 78, 91, 105, 120, 136,
     153, 171, 190, 210, 231, 253, 276, 300, 325, 351, 378, 406, 435, 465,
     496, 528, 561, 595, 630, 666, 703, 741, 780, 820, 861, 903, 946, 990,
     1035, 1081, 1128, 1176, 1225, 1275, 1326, 1378, 1431, 1485, 1540, 1596,
     1653, 1711, 1770, 1830, 1891, 1953, 2016, 2080, 2145, 2211, 2278, 2346,
     2415, 2485],
    [0, 0, 0, 1, 4, 10, 20, 35, 56, 84, 120, 165, 220, 286, 364, 455, 560,
     680, 816, 969, 1140, 1330, 1540, 1771, 2024, 2300, 2600, 2925, 3276,
     3654, 4060, 4495, 4960, 5456, 5984, 6545, 7140, 7770, 8436, 9139, 9880,
     10660, 11480, 12341, 13244, 14190, 15180, 16215, 17296, 18424, 19600,
     20825, 22100, 23426, 24804, 26235, 27720, 29260, 30856, 32509, 34220,
     35990, 37820, 39711, 41664, 43680, 45760, 47905, 50116, 52394, 54740,
     57155],
    [0, 0, 0, 0, 1, 5, 15, 35, 70, 126, 210, 330, 495, 715, 1001, 1365,
     1820, 2380, 3060, 3876, 4845, 5985, 7315, 8855, 10626, 12650, 14950,
     17550, 20475, 23751, 27405, 31465, 35960, 40920, 46376, 52360, 58905,
     66045, 73815, 82251, 91390, 101270, 111930, 123410, 135751, 148995,
     163185, 178365, 194580, 211876, 230300, 249900, 270725, 292825, 316251,
     341055, 367290, 395010, 424270, 455126, 487635, 521855, 557845, 595665,
     635376, 677040, 720720, 766480, 814385, 864501, 916895, 971635],
    [0, 0, 0, 0, 0, 1, 6, 21, 56, 126, 252, 462, 792, 1287, 2002, 3003,
     4368, 6188, 8568, 11628, 15504, 20349, 26334, 33649, 42504, 53130,
     65780, 80730, 98280, 118755, 142506, 169911, 201376, 237336, 278256,
     324632, 376992, 435897, 501942, 575757, 658008, 749398, 850668, 962598,
     1086008, 1221759, 1370754, 1533939, 1712304, 1906884, 2118760, 2349060,
     2598960, 2869685, 3162510, 3478761, 3819816, 4187106, 4582116, 5006386,
     5461512, 5949147, 6471002, 7028847, 7624512, 8259888, 8936928, 9657648,
     10424128, 11238513, 12103014, 13019909],
    [0, 0, 0, 0, 0, 0, 1, 7, 28, 84, 210, 462, 924, 1716, 3003, 5005,
     8008, 12376, 18564, 27132, 38760, 54264, 74613, 100947, 134596, 177100,
     230230, 296010, 376740, 475020, 593775, 736281, 906192, 1107568, 1344904,
     1623160, 1947792, 2324784, 2760681, 3262623, 3838380, 4496388, 5245786,
     6096454, 7059052, 8145060, 9366819, 10737573, 12271512, 13983816,
     15890700, 18009460, 20358520, 22957480, 25827165, 28989675, 32468436,
     36288252, 40475358, 45057474, 50063860, 55525372, 61474519, 67945521,
     74974368, 82598880, 90858768, 99795696, 109453344, 119877472, 131115985,
     143218999],
    [0, 0, 0, 0, 0, 0, 0, 1, 8, 36, 120, 330, 792, 1716, 3432, 6435,
     11440, 19448, 31824, 50388, 77520, 116280, 170544, 245157, 346104,
     480700, 657800, 888030, 1184040, 1560780, 2035800, 2629575, 3365856,
     4272048, 5379616, 6724520, 8347680, 10295472, 12620256, 15380937,
     18643560, 22481940, 26978328, 32224114, 38320568, 45379620, 53524680,
     62891499, 73629072, 85900584, 99884400, 115775100, 133784560, 154143080,
     177100560, 202927725, 231917400, 264385836, 300674088, 341149446,
     386206920, 436270780, 491796152, 553270671, 621216192, 696190560,
     778789440, 869648208, 969443904, 1078897248, 1198774720, 1329890705],
]

# Reflection coefficient codebook [14][32] (Q15, stored as int16)
FILTER_CB = [
    [-32653, -32587, -32515, -32438, -32341, -32216, -32062, -31881,
     -31665, -31398, -31080, -30724, -30299, -29813, -29248, -28572,
     -27674, -26439, -24666, -22466, -19433, -16133, -12218, -7783,
     -2834, 1819, 6544, 11260, 16050, 20220, 24774, 28120],
    [-27503, -24509, -20644, -17496, -14187, -11277, -8420, -5595,
     -3013, -624, 1711, 3880, 5844, 7774, 9739, 11592,
     13364, 14903, 16426, 17900, 19250, 20586, 21803, 23006,
     24142, 25249, 26275, 27300, 28359, 29249, 30118, 31183],
    [-27827, -24208, -20943, -17781, -14843, -11848, -9066, -6297,
     -3660, -910, 1918, 5025, 8223, 11649, 15086, 18423],
    [-17128, -11975, -8270, -5123, -2296, 183, 2503, 4707,
     6798, 8945, 11045, 13239, 15528, 18248, 21115, 24785],
    [-21557, -17280, -14286, -11644, -9268, -7087, -4939, -2831,
     -691, 1407, 3536, 5721, 8125, 10677, 13721, 17731],
    [-15030, -10377, -7034, -4327, -1900, 364, 2458, 4450,
     6422, 8374, 10374, 12486, 14714, 16997, 19626, 22954],
    [-16155, -12362, -9698, -7460, -5258, -3359, -1547, 219,
     1916, 3599, 5299, 6994, 8963, 11226, 13716, 16982],
    [-14742, -9848, -6921, -4648, -2769, -1065, 499, 2083,
     3633, 5219, 6857, 8580, 10410, 12672, 15561, 20101],
    [-11099, -7014, -3855, -1025, 1680, 4544, 7807, 11932],
    [-9060, -4570, -1381, 1419, 4034, 6728, 9865, 14149],
    [-12450, -7985, -4596, -1734, 961, 3629, 6865, 11142],
    [-11831, -7404, -4010, -1096, 1606, 4291, 7386, 11482],
    [-13404, -9250, -5995, -3312, -890, 1594, 4464, 8198],
    [-11239, -7220, -4040, -1406, 971, 3321, 6006, 9697],
]

FIXED_CB_GAIN = [
    0, 4, 8, 13, 17, 22, 26, 31, 35, 40, 44, 48, 53, 58, 63, 69,
    76, 83, 91, 99, 109, 119, 130, 142, 155, 170, 185, 203, 222, 242,
    265, 290, 317, 346, 378, 414, 452, 494, 540, 591, 646, 706, 771,
    843, 922, 1007, 1101, 1204, 1316, 1438, 1572, 1719, 1879, 2053,
    2244, 2453, 2682, 2931, 3204, 3502, 3828, 4184, 4574, 5000,
]

PULSE_VAL = [-31182, -22273, -13364, -4455, 4455, 13364, 22273, 31182]

BINARY_DECREASING = [
    32767, 16384, 8192, 4096, 2048, 1024, 512, 256, 128, 64, 32, 16, 8, 4, 2,
]

UNC_DECREASING = [
    32767, 26214, 20972, 16777, 13422, 10737, 8590, 6872,
    5498, 4398, 3518, 2815, 2252, 1801, 1441,
]

ADAPTIVE_GAIN = [
    102, 231, 360, 488, 617, 746, 875, 1004,
    1133, 1261, 1390, 1519, 1648, 1777, 1905, 2034,
    2163, 2292, 2421, 2550, 2678, 2807, 2936, 3065,
    3194, 3323, 3451, 3580, 3709, 3838, 3967, 4096,
]

SINC = [
    262, 293, 323, 348, 356, 336, 269, 139,
    -67, -358, -733, -1178, -1668, -2162, -2607, -2940,
    -3090, -2986, -2562, -1760, -541, 1110, 3187, 5651,
    8435, 11446, 14568, 17670, 20611, 23251, 25460, 27125,
    28160, 28512, 28160,
    27125, 25460, 23251, 20611, 17670, 14568, 11446, 8435,
    5651, 3187, 1110, -541, -1760, -2562, -2986, -3090,
    -2940, -2607, -2162, -1668, -1178, -733, -358, -67,
    139, 269, 336, 356, 348, 323, 293, 262,
]

# C(72,8) binomials for alternate pulse decoding mode
C72_BINOMIALS = [72, 2556, 59640, 1028790, 13991544, 156238908, 1473109704,
                 3379081753]


def _clip16(x):
    """Clip to int16 range [-32768, 32767]."""
    if x > 32767:
        return 32767
    if x < -32768:
        return -32768
    return int(x)


def _clip32767(x):
    """Clip to [-32767, 32767] matching DLL behavior."""
    if x > 32767:
        return 32767
    if x < -32767:
        return -32767
    return int(x)


def _formula(a, b, c):
    """DSS_SP_FORMULA: fixed-point MAC with rounding.
    ((a * 32768 + b * c) + 16384) >> 15
    """
    return int(((a * (1 << 15)) + b * c + 0x4000) >> 15)


# ==============================================================================
# DSS file reader
# ==============================================================================

def read_dss_file(path):
    """Read DSS file, extract frame packets via block-aware byte-swap demuxing.

    Handles empty blocks (frame_count=0) correctly by:
    1. Only including continuation bytes from empty block payloads
    2. Resetting swap state at block group boundaries using block headers

    Block header layout:
        byte0 bit7: swap state at block start
        byte1: first-frame offset = 2*byte1 + 2*swap (from block start)
        byte2: frame_count (frames starting in this block)

    Returns: (frame_packets, total_frames)
        frame_packets: list of 42-byte packets
    """
    with open(path, 'rb') as f:
        data = f.read()

    if data[1:4] != b'dss' or data[0] not in (2, 3):
        raise ValueError(f"Not a DSS file: {path}")

    version = data[0]
    header_size = version * DSS_BLOCK_SIZE
    num_blocks = (len(data) - header_size) // DSS_BLOCK_SIZE

    # Parse block headers
    blocks = []
    total_frames = 0
    for bi in range(num_blocks):
        bstart = header_size + bi * DSS_BLOCK_SIZE
        byte0 = data[bstart]
        byte1 = data[bstart + 1]
        frame_count = data[bstart + 2]
        blk_swap = (byte0 >> 7) & 1
        cont_size = max(0, 2 * byte1 + 2 * blk_swap - DSS_BLOCK_HEADER_SIZE)
        blocks.append((frame_count, blk_swap, cont_size,
                        data[bstart + DSS_BLOCK_HEADER_SIZE:bstart + DSS_BLOCK_SIZE]))
        total_frames += frame_count

    # Build stream: for empty blocks, only include continuation bytes.
    # Track positions where swap state needs resetting.
    stream = bytearray()
    swap_reset_positions = {}
    pos = 0
    for bi, (fc, blk_swap, cont_size, payload) in enumerate(blocks):
        if fc == 0:
            stream.extend(payload[:cont_size])
            pos += cont_size
            # Find next non-empty block and record its swap state
            for nbi in range(bi + 1, len(blocks)):
                if blocks[nbi][0] > 0:
                    swap_reset_positions[pos] = blocks[nbi][1]
                    break
        else:
            stream.extend(payload)
            pos += len(payload)

    # Byte-swap demuxing with swap resets at block group boundaries
    swap = blocks[0][1]
    swap_byte = 0
    pos = 0
    frame_packets = []

    for fi in range(total_frames):
        if pos in swap_reset_positions:
            swap = swap_reset_positions[pos]
            swap_byte = 0

        pkt = bytearray(DSS_SP_FRAME_SIZE + 1)
        if swap:
            read_size = 40
            end = min(pos + read_size, len(stream))
            pkt[3:3 + (end - pos)] = stream[pos:end]
            pos += read_size
            for i in range(0, DSS_SP_FRAME_SIZE - 2, 2):
                pkt[i] = pkt[i + 4]
            pkt[DSS_SP_FRAME_SIZE] = 0
            pkt[1] = swap_byte
        else:
            read_size = DSS_SP_FRAME_SIZE
            end = min(pos + read_size, len(stream))
            pkt[:end - pos] = stream[pos:end]
            pos += read_size
            swap_byte = pkt[DSS_SP_FRAME_SIZE - 2]
        pkt[DSS_SP_FRAME_SIZE - 2] = 0
        swap ^= 1
        frame_packets.append(bytes(pkt[:DSS_SP_FRAME_SIZE]))

    return frame_packets, total_frames


# ==============================================================================
# Bitstream reader (MSB-first within 16-bit LE words)
# ==============================================================================

class BitstreamReader:
    """MSB-first within 16-bit LE words, matching DssDecoder.dll FUN_10017460."""

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
# DSS SP Decoder
# ==============================================================================

class DSSDecoder:
    def __init__(self):
        self.excitation = [0] * (288 + 6)
        self.history = [0] * 187
        self.working_buffer = [[0] * 72 for _ in range(DSS_SP_SUBFRAMES)]
        self.audio_buf = [0] * 15
        self.err_buf1 = [0] * 15
        self.err_buf2 = [0] * 15
        self.lpc_filter = [0] * 14
        self.filter = [0] * 15
        self.vector_buf = [0] * 72
        self.noise_state = 0
        self.pulse_dec_mode = 1
        self.shift_amount = 0

    def _unpack_coeffs(self, pkt):
        """Unpack frame bitfields into parameters."""
        reader = BitstreamReader(pkt)

        # Reflection coefficient indices
        filter_idx = []
        for i in range(2):
            filter_idx.append(reader.read_bits(5))
        for i in range(6):
            filter_idx.append(reader.read_bits(4))
        for i in range(6):
            filter_idx.append(reader.read_bits(3))

        # Per-subframe parameters
        sf_adaptive_gain = []
        subframes = []
        for j in range(DSS_SP_SUBFRAMES):
            ag = reader.read_bits(5)
            sf_adaptive_gain.append(ag)
            combined_pulse_pos = reader.read_bits(31)
            gain = reader.read_bits(6)
            pulse_val = [reader.read_bits(3) for _ in range(7)]
            subframes.append({
                'combined_pulse_pos': combined_pulse_pos,
                'gain': gain,
                'pulse_val': pulse_val,
                'pulse_pos': [0] * 7,
            })

        # Decode pulse positions using combinatorial table
        for j in range(DSS_SP_SUBFRAMES):
            combined = subframes[j]['combined_pulse_pos']
            if combined < C72_BINOMIALS[7]:
                if self.pulse_dec_mode:
                    pulse = 7
                    pulse_idx = 71
                    cp = combined
                    for i in range(7):
                        while cp < COMBINATORIAL_TABLE[pulse][pulse_idx]:
                            pulse_idx -= 1
                        cp -= COMBINATORIAL_TABLE[pulse][pulse_idx]
                        pulse -= 1
                        subframes[j]['pulse_pos'][i] = pulse_idx
            else:
                self.pulse_dec_mode = 0
                c72 = list(C72_BINOMIALS)
                subframes[j]['pulse_pos'][6] = 0
                index = 6
                cp = combined
                for i in range(71, -1, -1):
                    if c72[index] <= cp:
                        cp -= c72[index]
                        subframes[j]['pulse_pos'][6 - index] = i
                        if index == 0:
                            break
                        index -= 1
                    c72[0] -= 1
                    if index:
                        for a in range(index):
                            c72[a + 1] -= c72[a]

        # Combined pitch (24 bits)
        combined_pitch = reader.read_bits(24)

        pitch_lag = [0] * DSS_SP_SUBFRAMES
        pitch_lag[0] = (combined_pitch % 151) + 36
        combined_pitch //= 151

        for i in range(1, DSS_SP_SUBFRAMES - 1):
            pitch_lag[i] = combined_pitch % 48
            combined_pitch //= 48
        pitch_lag[DSS_SP_SUBFRAMES - 1] = min(combined_pitch, 47)

        # Convert delta pitch to absolute
        pl = pitch_lag[0]
        for i in range(1, DSS_SP_SUBFRAMES):
            if pl > 162:
                pitch_lag[i] += 162 - 23
            else:
                tmp = pl - 23
                if tmp < 36:
                    tmp = 36
                pitch_lag[i] += tmp
            pl = pitch_lag[i]

        return filter_idx, sf_adaptive_gain, pitch_lag, subframes

    def _unpack_filter(self, filter_idx):
        """Look up reflection coefficients from codebook."""
        for i in range(14):
            self.lpc_filter[i] = FILTER_CB[i][filter_idx[i]]

    def _convert_coeffs(self):
        """Convert reflection coefficients to LPC polynomial (Levinson recursion).

        DLL-matching: detects overflow during recursion. If any coefficient
        overflows int16, restarts with halved precision (shift_amount=1).
        """
        self.shift_amount = 0
        self.filter[0] = 0x2000
        overflow = False
        for a in range(14):
            a_plus = a + 1
            self.filter[a_plus] = self.lpc_filter[a] >> 2
            for i in range(1, a_plus // 2 + 1):
                coeff_1 = self.filter[i]
                coeff_2 = self.filter[a_plus - i]
                tmp1 = _formula(coeff_1, self.lpc_filter[a], coeff_2)
                tmp2 = _formula(coeff_2, self.lpc_filter[a], coeff_1)
                if tmp1 > 32767 or tmp1 < -32768 or tmp2 > 32767 or tmp2 < -32768:
                    overflow = True
                self.filter[i] = _clip16(tmp1)
                self.filter[a_plus - i] = _clip16(tmp2)
        if overflow:
            self.shift_amount = 1
            self.filter[0] = 0x1000
            for a in range(14):
                a_plus = a + 1
                self.filter[a_plus] = self.lpc_filter[a] >> 3
                for i in range(1, a_plus // 2 + 1):
                    coeff_1 = self.filter[i]
                    coeff_2 = self.filter[a_plus - i]
                    self.filter[i] = _clip16(
                        _formula(coeff_1, self.lpc_filter[a], coeff_2))
                    self.filter[a_plus - i] = _clip16(
                        _formula(coeff_2, self.lpc_filter[a], coeff_1))

    def _gen_exc(self, pitch_lag, gain):
        """Generate pitch-adaptive excitation."""
        if pitch_lag < 72:
            for i in range(72):
                self.vector_buf[i] = self.history[pitch_lag - i % pitch_lag]
        else:
            for i in range(72):
                self.vector_buf[i] = self.history[pitch_lag - i]

        for i in range(72):
            tmp = gain * self.vector_buf[i] >> 11
            self.vector_buf[i] = _clip32767(tmp)

    def _add_pulses(self, sf):
        """Add fixed codebook pulses to excitation."""
        for i in range(7):
            pos = sf['pulse_pos'][i]
            val = (FIXED_CB_GAIN[sf['gain']] * PULSE_VAL[sf['pulse_val'][i]]
                   + 0x4000) >> 15
            self.vector_buf[pos] += val

    def _update_buf(self):
        """Update history buffer with current excitation."""
        for i in range(114, 0, -1):
            self.history[i + 72] = self.history[i]
        for i in range(72):
            self.history[72 - i] = self.vector_buf[i]

    def _scale_vector(self, vec, bits, size):
        """Scale vector by shifting."""
        if bits < 0:
            for i in range(size):
                vec[i] = int(vec[i]) >> (-bits)
        else:
            for i in range(size):
                vec[i] = int(vec[i]) * (1 << bits)

    def _get_normalize_bits(self, size):
        """Get normalization shift for vector_buf."""
        val = 1
        for i in range(size):
            val |= abs(int(self.vector_buf[i]))
        max_val = 0
        while val <= 0x4000:
            val *= 2
            max_val += 1
        return max_val

    def _vector_sum(self, size):
        """Sum of absolute values in vector_buf."""
        return sum(abs(int(self.vector_buf[i])) for i in range(size))

    def _vec_mult(self, src, mult):
        """Multiply filter coefficients by decreasing array."""
        dst = [0] * 15
        dst[0] = src[0]
        for i in range(1, 15):
            dst[i] = (src[i] * mult[i] + 0x4000) >> 15
        return dst

    def _shift_sq_add(self, filter_buf, audio_buf, dst):
        """LPC synthesis filter (forward)."""
        shift = 13 - self.shift_amount
        for a in range(72):
            audio_buf[0] = dst[a]
            tmp = 0
            for i in range(14, -1, -1):
                tmp += audio_buf[i] * filter_buf[i]
            for i in range(14, 0, -1):
                audio_buf[i] = audio_buf[i - 1]
            tmp = (tmp + 4096) >> shift
            dst[a] = _clip32767(tmp)

    def _shift_sq_sub(self, filter_buf, error_buf, dst):
        """Error correction filter (inverse)."""
        shift = 13 - self.shift_amount
        for a in range(72):
            tmp = dst[a] * filter_buf[0]
            for i in range(14, 0, -1):
                tmp -= error_buf[i] * filter_buf[i]
            for i in range(14, 0, -1):
                error_buf[i] = error_buf[i - 1]
            tmp = int(tmp + 4096) >> shift
            error_buf[1] = _clip32767(tmp)
            dst[a] = _clip32767(tmp)

    def _sf_synthesis(self, lpc_filter_0, dst, size):
        """Subframe synthesis with noise modulation."""
        vsum_1 = 0
        if size > 0:
            vsum_1 = self._vector_sum(size)
            if vsum_1 > 0xFFFFF:
                vsum_1 = 0xFFFFF

        normalize_bits = self._get_normalize_bits(size)

        self._scale_vector(self.vector_buf, normalize_bits - 3, size)
        self._scale_vector(self.audio_buf, normalize_bits, 15)
        self._scale_vector(self.err_buf1, normalize_bits, 15)

        v36 = self.err_buf1[1]

        tmp_buf = self._vec_mult(self.filter, BINARY_DECREASING)
        self._shift_sq_add(tmp_buf, self.audio_buf, self.vector_buf)

        tmp_buf = self._vec_mult(self.filter, UNC_DECREASING)
        self._shift_sq_sub(tmp_buf, self.err_buf1, self.vector_buf)

        lf = lpc_filter_0 >> 1
        if lf >= 0:
            lf = 0

        if size > 1:
            for i in range(size - 1, 0, -1):
                tmp = _formula(self.vector_buf[i], lf, self.vector_buf[i - 1])
                self.vector_buf[i] = _clip32767(tmp)

        tmp = _formula(self.vector_buf[0], lf, v36)
        self.vector_buf[0] = _clip32767(tmp)

        self._scale_vector(self.vector_buf, -normalize_bits, size)
        self._scale_vector(self.audio_buf, -normalize_bits, 15)
        self._scale_vector(self.err_buf1, -normalize_bits, 15)

        vsum_2 = 0
        if size > 0:
            vsum_2 = self._vector_sum(size)

        if vsum_2 >= 0x40:
            t = (vsum_1 << 11) // vsum_2
        else:
            t = 1

        bias = ((409 * t) >> 15) << 15
        noise = [0] * size
        tmp = (bias + 32358 * self.noise_state) >> 15
        noise[0] = _clip32767(tmp)

        for i in range(1, size):
            tmp = (bias + 32358 * noise[i - 1]) >> 15
            noise[i] = _clip32767(tmp)

        self.noise_state = noise[size - 1]

        for i in range(size):
            tmp = (self.vector_buf[i] * noise[i]) >> 11
            dst[i] = _clip32767(tmp)

    def _update_state(self, working_flat):
        """Sinc interpolation resampling 288 -> 264 samples (12000 -> 11025 Hz)."""
        for i in range(6):
            self.excitation[i] = self.excitation[288 + i]

        for i in range(288):
            self.excitation[6 + i] = working_flat[i]

        output = []
        offset = 6
        a = 0

        while offset < len(self.excitation):
            tmp = 0
            for i in range(6):
                idx = offset - i
                if 0 <= idx < len(self.excitation):
                    tmp += self.excitation[idx] * SINC[a + i * 11]
            offset += 1

            tmp >>= 15
            output.append(_clip16(tmp))

            a = (a + 1) % 11
            if a == 0:
                offset += 1

        # Truncate to 264 samples
        return output[:DSS_SP_SAMPLE_COUNT]

    def decode_frame(self, pkt):
        """Decode one DSS SP frame, returning 264 int16 samples."""
        filter_idx, sf_adaptive_gain, pitch_lag, subframes = \
            self._unpack_coeffs(pkt)

        self._unpack_filter(filter_idx)
        self._convert_coeffs()

        for j in range(DSS_SP_SUBFRAMES):
            self._gen_exc(pitch_lag[j], ADAPTIVE_GAIN[sf_adaptive_gain[j]])
            self._add_pulses(subframes[j])
            self._update_buf()

            for i in range(72):
                self.vector_buf[i] = self.history[72 - i]

            self._shift_sq_sub(self.filter, self.err_buf2, self.vector_buf)
            self._sf_synthesis(self.lpc_filter[0],
                               self.working_buffer[j], 72)

        # Flatten working buffer
        working_flat = []
        for j in range(DSS_SP_SUBFRAMES):
            working_flat.extend(self.working_buffer[j])

        # Sinc interpolation resample and produce 264 output samples
        output = self._update_state(working_flat)
        return output

    def decode_file(self, dss_path, wav_path=None):
        """Decode entire DSS file to samples."""
        frame_packets, total_frames = read_dss_file(dss_path)

        all_samples = []
        for fi in range(total_frames):
            samples = self.decode_frame(frame_packets[fi])
            all_samples.extend(samples)

        duration = len(all_samples) / DSS_SP_SAMPLE_RATE
        print(f"Decoded: {total_frames} frames, {duration:.2f}s at {DSS_SP_SAMPLE_RATE}Hz")

        samples_16 = np.array(all_samples, dtype=np.int16)

        if wav_path:
            with wave.open(wav_path, 'w') as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(DSS_SP_SAMPLE_RATE)
                w.writeframes(samples_16.tobytes())
            print(f"Written: {wav_path}")

        return samples_16


# ==============================================================================
# CLI
# ==============================================================================

if __name__ == '__main__':
    if len(sys.argv) < 2:
        print("Usage: dss_decode.py <input.DSS> [output.wav]")
        sys.exit(1)

    dss_path = sys.argv[1]
    if len(sys.argv) > 2:
        wav_path = sys.argv[2]
    else:
        wav_path = str(Path(dss_path).with_suffix('.decoded.wav'))

    decoder = DSSDecoder()
    samples = decoder.decode_file(dss_path, wav_path)
