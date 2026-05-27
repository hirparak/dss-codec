# DSS/DS2 Codec Specification

Complete technical specification for the Olympus DSS and DS2 proprietary speech codecs, reverse-engineered from `DssDecoder.dll` (AudioSDK, 465408 bytes) and `dss32.dll` (NCH Switch, 215936 bytes) using Ghidra, with verification against reference WAV output from Olympus DirectShow filters.

---

## Table of Contents

1. [Overview](#overview)
2. [File Formats](#file-formats)
3. [Encrypted Variant Notes](#encrypted-variant-notes)
4. [Bitstream Reader](#bitstream-reader)
5. [Demuxing](#demuxing)
6. [DSS SP Decoder](#dss-sp-decoder)
7. [DS2 SP Decoder](#ds2-sp-decoder)
8. [DS2 QP Decoder](#ds2-qp-decoder)
9. [Shared Algorithms](#shared-algorithms)
10. [All Quantization Tables](#all-quantization-tables)
11. [All Codebook Tables](#all-codebook-tables)
12. [Python Reference Decoders](#python-reference-decoders)
13. [Rust Implementation](#rust-implementation)
14. [Verification Results](#verification-results)
15. [DLL Function Map](#dll-function-map)

---

## Overview

The Olympus DSS/DS2 codec family implements **CELP (Code-Excited Linear Prediction)** speech coding in three quality modes:

| Mode | File Ext | Sample Rate | Frame Bits | Frame Samples | Frame Duration | Bit Rate |
|------|----------|-------------|------------|---------------|----------------|----------|
| DSS SP | .dss | 12000 Hz (output: 11025 Hz) | 328 | 264 (after resample) | 24ms | ~13.7 kbps |
| DS2 SP | .ds2 | 12000 Hz | 328 | 288 | 24ms | ~13.7 kbps |
| DS2 QP | .ds2 | 16000 Hz | 448 | 256 | 16ms | ~28 kbps |

All three are mono speech codecs designed for dictation recording on Olympus devices (DS-series, DM-series, DPM-series recorders).

### Architecture Summary

```
Bitstream → Demux → Unpack Frame → {
    1. Dequantize reflection coefficients (from codebook)
    2. Per subframe:
       a. Decode pitch lag
       b. Generate adaptive excitation (pitch memory lookup)
       c. Generate fixed excitation (combinatorial codebook + pulse amplitudes)
       d. Combine: excitation = gp * adaptive + gc * fixed
       e. Synthesis filter (LPC or lattice)
       f. Update pitch memory
    3. Post-processing (noise modulation / de-emphasis / resampling)
}
```

### Key Differences from FFmpeg

FFmpeg's built-in `dss_sp` decoder does NOT work for DS2 files:
- FFmpeg uses 11025 Hz; Olympus SP uses 12000 Hz natively
- Completely different lookup tables — zero overlap
- Different frame structure (FFmpeg: 264 bits, Olympus SP: 328 bits)
- Correlation between FFmpeg and correct output: ~0.01 (random noise)

---

## File Formats

### DSS File Header

| Offset | Size | Description |
|--------|------|-------------|
| 0x000 | 1 | Version: `0x02` (DSS v2) or `0x03` (DSS v3) |
| 0x001 | 3 | Magic: `dss` |
| 0x004+ | varies | Metadata (author, timestamps, etc.) |

Header size = `version * 512` bytes (v2: 1024, v3: 1536).

### DS2 File Header (1536 bytes = 0x600)

| Offset | Size | Description |
|--------|------|-------------|
| 0x000 | 4 | Magic: `\x03ds2` |
| 0x004 | 2 | Version (02 00) |
| 0x006 | 2 | Quality setting (03 00) |
| 0x00C | 16 | Author ID (alphanumeric string) |
| 0x026 | 24 | Timestamps (ASCII) |
| 0x3E6 | 10 | Device serial number |
| 0x600 | ... | Audio data begins |

### Audio Block Structure (shared DSS/DS2)

All audio data is organized in 512-byte blocks:

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Byte 0: bit 7 = swap state for byte-swap demuxing |
| 1 | 1 | Byte 1: continuation data offset (in words) |
| 2 | 1 | Frame count in this block |
| 3 | 1 | 0xFF marker |
| 4 | 1 | Format type: 0x00 = SP, 0x06 = QP |
| 5 | 1 | 0xFF marker |
| 6-511 | 506 | Audio payload |

Frame count in block headers sums to total frame count for the file.

---

  ## Encrypted Variant Notes

  ### High-Level Overview

  Encrypted DS2 is not a different container format. It is normal DS2 with:

  - file magic changed from \x03ds2 to \x03enc
  - a 22-byte decrypt descriptor embedded in the header
  - each 512-byte audio block transformed only over a 496-byte window

  The block structure and DS2 framing remain intact. The 6-byte DS2 block header is left in clear, and the final 10
  bytes of each 512-byte block are outside the transform window.

  Cryptographically, the payload transform is:

  - password-derived AES key material
  - applied to each block as a custom self-rekeying AES decrypt mode
  - with an adjacent-byte swap before and after the AES step

  The implementation problem is not "decrypt an entire file with CBC/CTR/etc." It is:

  1. parse the descriptor
  2. derive the initial AES key from password + aux_16
  3. initialize a small saved-state blob
  4. for each 512-byte DS2 block, transform only bytes 6..501
  5. after each 16-byte AES block, derive the next AES key from the current AES state

  ### File-Level Structure

  - Plain DS2 magic: \x03ds2
  - Encrypted DS2 magic: \x03enc
  - Header size remains 0x600
  - Audio still begins at 0x600
  - Audio remains organized as 512-byte blocks

  Each encrypted block keeps the normal 6-byte DS2 block header unchanged:

  | Offset | Size | Meaning |
  |---|---:|---|
  | 0..5 | 6 | Normal DS2 block header |
  | 6..501 | 496 (0x1f0) | Encrypted/transformed payload window |
  | 502..511 | 10 | Untouched payload tail |

  Important consequence:

  - the logical DS2 payload area is still 506 bytes wide
  - but only the first 496 bytes of that area are transformed
  - bytes 502..511 are not part of the crypto window

  ### Decrypt Descriptor

  Encrypted DS2 stores a 22-byte decrypt descriptor at header offset 0x146.

  | Relative Offset | Size | Meaning |
  |---|---:|---|
  | 0x00..0x01 | 2 | Mode: 0x0001 = AES-128, 0x0002 = AES-256 |
  | 0x02..0x11 | 16 | aux_16 block |
  | 0x12..0x13 | 2 | Expected check word, little-endian |
  | 0x14..0x15 | 2 | Present in the 22-byte descriptor, not required for decryption |

  For a practical decoder, the useful fields are:

  - key mode
  - aux_16
  - expected check word

  ### Password Mixing and Key Derivation

  Passwords are treated as raw bytes and are limited to 16 bytes.

  Derivation process:

  1. Zero-pad the password to 16 bytes.
  2. XOR the padded password with aux_16.
  3. Hash the resulting 16-byte mixed block.

  For AES-128 (mode = 1):

  - digest = SHA-1(mixed_16)
  - key = digest[0..15]
  - check dword = little-endian digest[16..19]
  - compared check word = low 16 bits of that dword

  For AES-256 (mode = 2):

  - digest = SHA-384(mixed_16)
  - key = digest[0..31]
  - check dword = little-endian digest[32..35]
  - compared check word = low 16 bits of that dword

  The derived low-16-bit check word must match the descriptor before any payload block is decrypted.

  ### Saved Decrypt State

  The vendor builds and saves a 0x12c-byte decrypt-state blob.

  Useful layout:

  - +0x000 .. +0x00f : zeroed prefix
  - +0x010 .. +0x01f : current plaintext 16-byte block
  - +0x020 .. +0x127 : AES state / schedule blob
  - +0x120 : AES round count
  - +0x124 : flags (0x12 when ready)
  - +0x128 : block byte index

  Initialization behavior:

  - zero +0x000 .. +0x00f
  - set block_byte_index = 0x10
  - expand the derived AES key into the AES state area at +0x20

  ### Per-Block Transform

  For each 512-byte DS2 block:

  1. Take bytes 6..501 as the 496-byte transformed window.
  2. Byte-swap each adjacent pair in that window.
  3. Run the self-rekeying AES decrypt loop.
  4. Byte-swap each adjacent pair again.
  5. Write the result back to bytes 6..501.
  6. Leave bytes 0..5 and 502..511 unchanged.

  This means the transformed window is exactly 31 AES blocks of 16 bytes.

  ### Self-Rekeying AES Loop

  The transformed 496-byte window is processed as 31 AES blocks.

  For each 16-byte chunk:

  1. AES-decrypt the ciphertext block with the current key state.
  2. Store the plaintext block at saved-state offset +0x10.
  3. Derive the next AES key from the current AES state blob.
  4. Expand/install that next key immediately.
  5. Output the plaintext block.

  The rekey source begins at:

  (round_count + 2) * 0x10

  within the saved-state blob.

  That formula is important. It is not arbitrary:

  - for AES-128 (round_count = 10), it points at state + 0x0c0
  - for AES-256 (round_count = 14), it points at state + 0x100

  Those locations correspond to the final-round key words in the vendor AES state blob.

  #### AES-128 Rekey

  - read 16 bytes from the rekey source
  - use those 16 bytes directly as the next AES-128 key

  #### AES-256 Rekey

  1. Read 16 bytes from the rekey source as four big-endian 32-bit words:

  [w0, w1, w2, w3]

  2. Form the next 32-byte key as:

  [w0, w1, w2, w3, w1, w0, w3, w2]

  3. Serialize those eight words back as big-endian 32-bit words.
  4. Expand/install that as the next AES-256 key.

  This reshuffle is one of the main non-obvious details that must be implemented exactly.

  ### AES State Representation

  The saved-state AES words are stored in big-endian word order.

  The vendor's in-memory AES blob is not just a textbook expanded key array:

  - the first key words are stored raw
  - the final round-key words are stored raw
  - middle-round material is stored in a T-table-oriented decrypt form

  For a clean decoder, you do not have to reproduce that vendor blob byte-for-byte if your AES decrypt
  implementation produces the same block outputs. A normal AES key expansion plus inverse-round decrypt
  implementation is sufficient for a clean implementation.

  But for byte-level oracle matching, the vendor blob layout matters.

  ### Important Non-Findings / Things Not To Overcomplicate

  - The last 10 bytes of each 512-byte block are not part of the transform window.
  - The 6-byte DS2 block header is not encrypted.
  - This is not CBC, CTR, OFB, or ECB over the full block stream.
  - The file is not reorganized; it is still normal DS2 framing with an encrypted payload window.

  ### Decoder Integration

  After block decryption:

  - rewrite file magic from \x03enc to \x03ds2
  - leave header structure otherwise intact
  - decrypt each 512-byte audio block as above
  - feed the result into the normal DS2 demux/decode path

  ### Practical Implementation Notes

  These are the details most likely to save another developer time:

  - Treat the password as raw bytes, not UTF-16 text.
  - Enforce the 16-byte password cap.
  - Parse the descriptor from header offset 0x146.
  - Compare only the low 16 bits of the derived check dword.
  - Transform only bytes 6..501.
  - Swap adjacent bytes before and after the AES loop.
  - Maintain the saved-state block byte index at +0x128.
  - Rekey after every 16-byte AES block.
  - Read/write AES rekey words as big-endian 32-bit values.
  - For AES-256, do not miss the [w1,w0,w3,w2] second half.
  - Do not assume bytes 502..511 carry encrypted audio payload.

---

## Bitstream Reader

All three codecs use the same bitstream reading convention:

**MSB-first within 16-bit little-endian words.**

The raw byte stream is interpreted as a sequence of 16-bit words in little-endian byte order. Within each word, bits are read from MSB (bit 15) to LSB (bit 0). When a word is exhausted, the next 16-bit LE word is loaded.

### Algorithm (DLL: FUN_10017460)

```
state: word_index, mask, current_word

read_bit():
    if mask == 0:
        mask = 0x8000
        current_word = load_16bit_LE(data[word_index * 2])
        word_index += 1
    else:
        mask >>= 1
        if mask == 0:
            mask = 0x8000
            current_word = load_16bit_LE(data[word_index * 2])
            word_index += 1
    bit = 1 if (current_word & mask) else 0
    return bit

read_bits(n):
    result = 0
    result_mask = 1 << (n - 1)
    for i in 0..n:
        if read_bit():
            result |= result_mask
        result_mask >>= 1
    return result
```

The first call to `read_bit()` initializes `mask` to 0x8000 and loads the first word. Subsequent calls shift `mask` right. When `mask` reaches 0, a new word is loaded.

---

## Demuxing

### DS2 SP Byte-Swap Demuxing

DS2 SP mode uses an interleaved byte-swap scheme identical to FFmpeg's `dss.c` demuxer (but with different codec parameters). Frames are extracted from the continuous block payload stream via alternating 42-byte and 40-byte reads with byte shuffling.

```
swap = (block0_byte0 >> 7) & 1   // Initial swap state from first block header
swap_byte = 0
pos = 0

for each frame:
    pkt = [0] * 43   // 42 + 1 working bytes

    if swap:
        read 40 bytes from stream into pkt[3..43]
        for i in 0..40 step 2:
            pkt[i] = pkt[i + 4]    // Shift even bytes down by 4
        pkt[42] = 0
        pkt[1] = swap_byte         // Carry byte from previous non-swap frame
    else:
        read 42 bytes from stream into pkt[0..42]
        swap_byte = pkt[40]         // Save byte for next swap frame

    pkt[40] = 0
    swap ^= 1                       // Toggle swap state

    output pkt[0..42]               // 42-byte frame packet
```

### DSS Block-Aware Demuxing

DSS files have a critical complication: **empty blocks** (frame_count = 0). These must be handled specially:

1. Empty block payloads contain only continuation data (partial frame bytes), not full frames
2. Continuation size: `cont_size = 2 * byte1 + 2 * swap - 6` bytes of valid data
3. Remaining payload bytes in empty blocks are garbage and must be discarded
4. Swap state resets at block group boundaries (from next non-empty block's byte0 bit 7)
5. FFmpeg's `dss.c` demuxer does NOT handle empty blocks — it reads straight through, producing corrupt output

### DS2 QP Continuous Demuxing

QP mode is simpler: the payload from all blocks is concatenated into a continuous bitstream with no byte-swap. Frames are read sequentially from this bitstream using the standard bitstream reader.

The 28-block cycle produces exactly 253 frames: block 0 has 10 frames, blocks 1-27 have 9 frames each. `28 * 506 bytes = 253 * 56 bytes` (448 bits/frame = 56 bytes/frame).

---

## DSS SP Decoder

The most complex decoder. Uses Q15 fixed-point integer arithmetic throughout, matching the behavior of the original Olympus DLL exactly.

### Parameters (DLL: FUN_100180c0)

| Parameter | Value | Source |
|-----------|-------|--------|
| Sample rate (native) | 12000 Hz | state[0x08] |
| Output sample rate | 11025 Hz | After sinc resampling |
| LPC order | 12 | state[0x00] |
| Num reflection coefficients | 14 | state[0x1C] |
| Num subframes | 4 | computed: ftol(24/6.0) |
| Subframe size | 72 samples | computed: 12 * 6.0 |
| Min pitch lag | 36 | state[0x38] |
| Max pitch lag | 186 | state[0x3C] |
| Pitch delta range | 48 | state[0x40] |
| Pitch range | 151 | 186 - 36 + 1 |
| Excitation pulses | 7 | state[0x68] |
| Samples per frame | 288 (before resample) | 72 * 4 |
| Output samples per frame | 264 (after resample) | 288 * 11/12 |
| Frame duration | 24 ms | 264 / 11025 |

### Frame Bit Allocation (328 bits total)

```
Reflection coefficients:         52 bits
    coeffs[0..1]:  2 x 5 bits = 10 bits
    coeffs[2..7]:  6 x 4 bits = 24 bits
    coeffs[8..13]: 6 x 3 bits = 18 bits

Per subframe (x4):               63 bits each = 252 bits
    adaptive gain index:          5 bits
    combinatorial CB index:      31 bits  [ceil(log2(C(72,7))) = 31]
    fixed CB gain index:          6 bits
    pulse amplitudes:       7 x 3 bits = 21 bits

Combined pitch:                  24 bits
    Encodes 4 pitch lags via divmod: range = 151 * 48^3 = 16,699,392

Total: 52 + 252 + 24 = 328 bits = 41 bytes
```

### Decoder State

```
excitation[294]:     i64   // Excitation history (288 + 6 overlap for sinc filter)
history[187]:        i64   // Pitch prediction buffer
working_buffer[4][72]: i64 // Per-subframe output before resampling
audio_buf[15]:       i64   // LPC synthesis filter state (shift_sq_add)
err_buf1[15]:        i64   // Error correction filter state (shift_sq_sub, UNC)
err_buf2[15]:        i64   // Error correction filter state (shift_sq_sub, first pass)
lpc_filter[14]:      i64   // Raw reflection coefficients from codebook
filter[15]:          i64   // LPC polynomial coefficients (from Levinson)
vector_buf[72]:      i64   // Working buffer for current subframe
noise_state:         i64   // PRNG state for noise modulation
pulse_dec_mode:      bool  // Pulse decoding mode flag
shift_amount:        i32   // 0 or 1, set by overflow detection in Levinson
```

### Processing Pipeline

#### Step 1: Unpack Coefficients (`_unpack_coeffs`)

1. Read 14 reflection coefficient indices from bitstream (bit allocations: 5,5,4,4,4,4,4,4,3,3,3,3,3,3)
2. Per subframe (x4): read adaptive_gain(5), combined_pulse_pos(31), fixed_gain(6), pulse_vals(7x3)
3. Decode pulse positions from combined_pulse_pos using the combinatorial number system:
   - Primary mode: iterate from C(71,7) downward, subtracting C(n,k) values
   - Fallback mode (if combined >= C(72,7)): alternate algorithm using running binomial updates
   - Positions are returned in **descending order** — must NOT sort
4. Read combined pitch (24 bits) and decode via divmod:
   - `pitch[0] = combined % 151 + 36`
   - `remaining = combined / 151`
   - `pitch[1] = remaining % 48`, `remaining /= 48`
   - `pitch[2] = remaining % 48`, `remaining /= 48`
   - `pitch[3] = min(remaining, 47)`
   - Convert deltas to absolute: `base = max(36, min(prev - 23, 162 - 23))`, `pitch[i] = base + delta[i]`

#### Step 2: Unpack Filter (`_unpack_filter`)

Look up each reflection coefficient index in the FILTER_CB codebook to get Q15 integer values:

```
for i in 0..14:
    lpc_filter[i] = FILTER_CB[i][filter_idx[i]]
```

#### Step 3: Convert Coefficients — Levinson Recursion (`_convert_coeffs`)

Convert reflection coefficients to LPC polynomial coefficients using the Levinson-Durbin algorithm with overflow detection:

```
shift_amount = 0
filter[0] = 0x2000   // Q13 unity

for a in 0..14:
    filter[a+1] = lpc_filter[a] >> 2

    for i in 1..=(a+1)/2:
        coeff_1 = filter[i]
        coeff_2 = filter[a+1-i]
        tmp1 = formula(coeff_1, lpc_filter[a], coeff_2)
        tmp2 = formula(coeff_2, lpc_filter[a], coeff_1)

        if tmp1 or tmp2 overflow [-32768, 32767]:
            overflow = true

        filter[i] = clip16(tmp1)
        filter[a+1-i] = clip16(tmp2)

if overflow:
    // Restart with halved precision
    shift_amount = 1
    filter[0] = 0x1000
    // Repeat loop with >> 3 instead of >> 2, no overflow check
```

Where `formula(a, b, c) = (a * 32768 + b * c + 16384) >> 15`.

The `shift_amount` flag (0 or 1) controls the shift value used in all subsequent filter operations: `shift = 13 - shift_amount`.

#### Step 4: Per-Subframe Processing

For each of the 4 subframes:

**4a. Generate Adaptive Excitation (`_gen_exc`)**

```
if pitch_lag < 72:
    // Short pitch: repeat cyclically
    for i in 0..72:
        vector_buf[i] = history[pitch_lag - (i % pitch_lag)]
else:
    // Normal pitch: single lookup
    for i in 0..72:
        vector_buf[i] = history[pitch_lag - i]

// Scale by adaptive gain
for i in 0..72:
    vector_buf[i] = clip32767((ADAPTIVE_GAIN[gain_idx] * vector_buf[i]) >> 11)
```

**4b. Add Fixed Codebook Pulses (`_add_pulses`)**

```
for i in 0..7:
    pos = pulse_pos[i]
    val = (FIXED_CB_GAIN[gain] * PULSE_VAL[pulse_val[i]] + 0x4000) >> 15
    vector_buf[pos] += val
```

**4c. Update History Buffer (`_update_buf`)**

```
// Shift history right by 72
for i in 114 downto 1:
    history[i + 72] = history[i]

// Copy excitation into history (reversed)
for i in 0..72:
    history[72 - i] = vector_buf[i]
```

**4d. First Error Correction Filter (`_shift_sq_sub` with err_buf2)**

```
shift = 13 - shift_amount
for a in 0..72:
    tmp = vector_buf[a] * filter[0]
    for i in 14 downto 1:
        tmp -= err_buf2[i] * filter[i]
    shift err_buf2 right by 1
    tmp = (tmp + 4096) >> shift
    err_buf2[1] = clip32767(tmp)
    vector_buf[a] = clip32767(tmp)
```

**4e. Noise Modulation Synthesis (`_sf_synthesis`)**

This is the most complex step, involving:

1. **Energy measurement**: `vsum_1 = sum(|vector_buf[i]|)`, clamped to 0xFFFFF

2. **Normalization**: Find leading zeros of max(|vector_buf|) to determine normalize_bits. Scale vector_buf up by `normalize_bits - 3`, scale audio_buf and err_buf1 up by `normalize_bits`.

3. **LPC Synthesis filter** (`_shift_sq_add` with BINARY_DECREASING):
   ```
   tmp_buf[i] = (filter[i] * BINARY_DECREASING[i] + 0x4000) >> 15
   for each sample:
       audio_buf[0] = vector_buf[a]
       tmp = sum(audio_buf[i] * tmp_buf[i])
       shift audio_buf right
       vector_buf[a] = clip32767((tmp + 4096) >> shift)
   ```

4. **Error correction filter** (`_shift_sq_sub` with UNC_DECREASING):
   ```
   tmp_buf[i] = (filter[i] * UNC_DECREASING[i] + 0x4000) >> 15
   for each sample:
       tmp = vector_buf[a] * tmp_buf[0]
       tmp -= sum(err_buf1[i] * tmp_buf[i])
       shift err_buf1 right
       err_buf1[1] = clip32767((tmp + 4096) >> shift)
       vector_buf[a] = clip32767((tmp + 4096) >> shift)
   ```

5. **Noise modulation LPC**:
   ```
   lf = min(0, lpc_filter[0] >> 1)
   for i in 71 downto 1:
       vector_buf[i] = clip32767(formula(vector_buf[i], lf, vector_buf[i-1]))
   vector_buf[0] = clip32767(formula(vector_buf[0], lf, prev_err_buf1[1]))
   ```

6. **Scale down**: Reverse the normalization scaling.

7. **Energy ratio and PRNG noise**:
   ```
   vsum_2 = sum(|vector_buf[i]|)
   t = (vsum_1 << 11) / max(vsum_2, 0x40)
   bias = ((409 * t) >> 15) << 15

   noise[0] = clip32767((bias + 32358 * noise_state) >> 15)
   for i in 1..72:
       noise[i] = clip32767((bias + 32358 * noise[i-1]) >> 15)
   noise_state = noise[71]

   working_buffer[sf][i] = clip32767((vector_buf[i] * noise[i]) >> 11)
   ```

#### Step 5: Sinc Resampling — 12000 to 11025 Hz (`_update_state`)

The 288 working samples (4 subframes x 72) are resampled to 264 output samples using an 11:12 ratio polyphase sinc interpolation filter with 67 coefficients:

```
// Copy working buffer into excitation (with 6-sample overlap from previous frame)
excitation[0..6] = excitation[288..294]     // Overlap from previous frame
excitation[6..294] = working_buffer_flat[0..288]

offset = 6
a = 0    // Phase index (0..10, cycles through 11 phases)

while offset < 294:
    tmp = 0
    for i in 0..6:
        tmp += excitation[offset - i] * SINC[a + i * 11]
    offset += 1
    output = clip16(tmp >> 15)

    a = (a + 1) % 11
    if a == 0:
        offset += 1    // Skip one input sample every 11 outputs (11:12 ratio)

// Output is truncated to 264 samples
```

The SINC table has 67 elements (6 taps x 11 phases + 1), stored as Q15 integers.

#### Clipping Functions

```
clip16(x)    = clamp(x, -32768, 32767)     // Standard 16-bit
clip32767(x) = clamp(x, -32767, 32767)     // Symmetric +-32767 (NOT +-32768!)
formula(a, b, c) = (a * 32768 + b * c + 16384) >> 15
```

The use of `clip32767` (+-32767, not +-32768) is critical for matching DLL output exactly.

---

## DS2 SP Decoder

Uses f64 floating-point arithmetic with normalized lattice synthesis filter. Structurally simpler than DSS SP.

### Parameters

| Parameter | Value |
|-----------|-------|
| Sample rate | 12000 Hz |
| Reflection coefficients | 14 |
| Subframes | 4 |
| Subframe size | 72 samples |
| Min pitch | 36 |
| Max pitch | 186 |
| Pitch range | 151 |
| Pitch delta range | 48 |
| Excitation pulses | 7 |
| Samples per frame | 288 |
| Frame duration | 24 ms |
| Frame bits | 328 |

### Frame Bit Allocation (328 bits)

```
Reflection coefficients:                52 bits
    coeffs[0..1]:  2 x 5 bits  = 10
    coeffs[2..7]:  6 x 4 bits  = 24
    coeffs[8..13]: 6 x 3 bits  = 18

Per subframe (x4):                      63 bits each = 252 bits
    pitch gain index:                    5 bits
    combinatorial CB index:             31 bits  [ceil(log2(C(72,7)))]
    excitation gain index:               6 bits
    pulse amplitudes:             7 x 3 = 21 bits

Combined pitch (at end of frame):       24 bits

Total: 52 + 252 + 24 = 328 bits
```

Note: The combined pitch field is read **after** all 4 subframes' per-subframe data (at the end of the frame), unlike DSS SP where it's interleaved.

### Decoder State

```
lattice_state[14]:   f64    // Lattice synthesis filter state
pitch_memory[258]:   f64    // Pitch prediction memory (max_pitch + subframe_size)
```

### Processing Pipeline

For each 42-byte packet:

1. **Read reflection coefficient indices** (14 values, bit allocs: 5,5,4,4,4,4,4,4,3,3,3,3,3,3)
2. **Read per-subframe data** (x4): pg_idx(5), cb_idx(31), gain_idx(6), pulses(7x3)
3. **Read combined pitch** (24 bits at end of frame)
4. **Decode pitch lags** using `decode_combined_pitch()` (see Shared Algorithms)
5. **Dequantize reflection coefficients**: `coeffs[i] = SP_CODEBOOK_i[index]` (f64 lookup)

For each subframe:

6. **Adaptive excitation**:
   ```
   gp = SP_PITCH_GAIN[pg_idx]
   for i in 0..72:
       if pitch < 72:
           adaptive_exc[i] = pitch_memory[mem_len - pitch + (i % pitch)]
       else:
           adaptive_exc[i] = pitch_memory[mem_len - pitch + i]
   ```

7. **Fixed codebook excitation**:
   ```
   gc = SP_EXCITATION_GAIN[gain_idx]
   positions = decode_combinatorial_index(cb_idx, 72, 7)   // Descending order!
   for pi, pos in positions:
       fixed_exc[pos] += SP_PULSE_AMP[pulses[pi]] * gc
   ```

8. **Total excitation**: `excitation[i] = gp * adaptive_exc[i] + fixed_exc[i]`

9. **Lattice synthesis**: `output = lattice_synthesis(excitation, coeffs, lattice_state)`

10. **Update pitch memory**: Shift left by 72, append excitation to end.

---

## DS2 QP Decoder

Higher quality mode with 16 reflection coefficients and per-subframe pitch encoding.

### Parameters (DLL: FUN_100179d0 + FUN_10017a80)

| Parameter | Value |
|-----------|-------|
| Sample rate | 16000 Hz |
| Reflection coefficients | 16 |
| Subframes | 4 |
| Subframe size | 64 samples |
| Min pitch | 45 |
| Max pitch | 300 |
| Pitch delta range | 256 |
| Excitation pulses | 11 |
| Samples per frame | 256 |
| Frame duration | 16 ms |
| Frame bits | 448 |

### Frame Bit Allocation (448 bits)

```
Reflection coefficients:                  76 bits
    coeffs[0..1]:   2 x 7 bits  = 14
    coeffs[2..3]:   2 x 6 bits  = 12
    coeffs[4..8]:   5 x 5 bits  = 25
    coeffs[9..12]:  4 x 4 bits  = 16
    coeffs[13..14]: 2 x 3 bits  =  6
    coeffs[15]:     1 x 3 bits  =  3

Per subframe (x4):                        93 bits each = 372 bits
    pitch index:                           8 bits  (absolute: pitch = index + 45)
    pitch gain index:                      6 bits
    combinatorial CB index:               40 bits  [ceil(log2(C(64,11)))]
    excitation gain index:                 6 bits
    pulse amplitudes:              11 x 3 = 33 bits

Total: 76 + 372 = 448 bits = 56 bytes
```

Key difference from SP: QP uses **absolute per-subframe pitch encoding** (8 bits each, pitch = index + 45), not combined pitch with delta encoding.

### Decoder State

```
lattice_state[16]:   f64    // Lattice synthesis filter state
pitch_memory[364]:   f64    // Pitch prediction memory (max_pitch + subframe_size)
deemph_state:        f64    // De-emphasis filter state
```

### Processing Pipeline

1. **Read reflection coefficient indices** (16 values, bit allocs: 7,7,6,6,5,5,5,5,5,4,4,4,4,3,3,3)
2. **Read per-subframe data** (x4): pitch_idx(8), pg_idx(6), cb_idx(40), gain_idx(6), pulses(11x3)
3. **Pitch**: `pitch = pitch_idx + 45` (absolute, per subframe)
4. **Dequantize reflection coefficients**: `coeffs[i] = QP_CODEBOOK_i[index]`

For each subframe (identical structure to DS2 SP but with different sizes):

5. **Adaptive excitation** (same as SP but subframe_size=64, max_pitch=300)
6. **Fixed codebook excitation**: `decode_combinatorial_index(cb_idx, 64, 11)` — 11 pulses, 64 positions
7. **Total excitation**: `excitation[i] = gp * adaptive_exc[i] + fixed_exc[i]`
8. **Lattice synthesis**: `output = lattice_synthesis(excitation, coeffs, lattice_state)`
9. **Update pitch memory**

### De-emphasis Filter

Applied to the entire decoded stream **after all frames** are decoded (not per-frame):

```
alpha = 0.1    // DLL: DAT_10065988

y[0] = x[0] + alpha * deemph_state
for n in 1..total_samples:
    y[n] = x[n] + alpha * y[n-1]

deemph_state = y[total_samples - 1]
```

This is a simple first-order IIR high-pass boost filter that compensates for pre-emphasis applied during encoding.

---

## Shared Algorithms

### Combinatorial Number System Decode

Used to decode compactly encoded pulse positions. Given an index and parameters (n, k), returns k positions from {0..n-1} in **descending order**.

For SP: C(72, 7) = 1,473,109,704 (31 bits)
For QP: C(64, 11) = 743,595,781,824 (40 bits)

```python
def decode_combinatorial_index(index, n, k):
    positions = []
    remaining = index

    for i in range(k, 0, -1):    # k down to 1
        v = i - 1
        while v + 1 < n and comb(v + 1, i) <= remaining:
            v += 1
        positions.append(v)
        remaining -= comb(v, i)

    return positions   # DESCENDING ORDER — do NOT sort!
```

The positions are in descending order because the combinatorial encoding maps the k-subset {p_k > p_{k-1} > ... > p_1} to `sum(C(p_i, i))`. The decode naturally produces positions from largest to smallest.

### Binomial Coefficient

```python
def comb(n, k):
    if k > n: return 0
    if k == 0 or k == n: return 1
    k = min(k, n - k)
    result = 1
    for i in range(k):
        result = result * (n - i) // (i + 1)
    return result
```

In Rust, u128 intermediates are used to avoid overflow for large values like C(64, 11).

### Combined Pitch Decode (SP only)

DS2 SP and DSS SP encode 4 pitch lags into a single 24-bit value:

```python
def decode_combined_pitch(combined, pitch_range, min_pitch, delta_range, num_subframes):
    # First pitch: absolute
    p0_idx = combined % pitch_range    # pitch_range = 151
    remaining = combined // pitch_range

    # Subsequent pitches: delta-encoded
    deltas = []
    for _ in range(num_subframes - 2):
        deltas.append(remaining % delta_range)  # delta_range = 48
        remaining //= delta_range
    deltas.append(min(remaining, delta_range - 1))

    # Convert deltas to absolute pitches
    pitches = [p0_idx + min_pitch]     # min_pitch = 36
    half_delta = delta_range // 2 - 1  # = 23
    max_pitch = min_pitch + pitch_range - 1  # = 186
    upper_limit = max_pitch - half_delta     # = 163

    for delta_idx in deltas:
        prev = pitches[-1]
        if prev > upper_limit:
            base = upper_limit - half_delta     # = 140
        elif prev >= min_pitch + half_delta:    # >= 59
            base = prev - half_delta
        else:
            base = min_pitch                    # = 36
        pitches.append(base + delta_idx)

    return pitches
```

### Normalized Lattice Synthesis Filter

Used by both DS2 SP and DS2 QP. Matches DLL FUN_10019d40 / FUN_10019060:

```python
def lattice_synthesis(excitation, coeffs, state):
    p = len(coeffs)      # 14 for SP, 16 for QP
    output = [0.0] * len(excitation)

    for n in range(len(excitation)):
        # Start from bottom of lattice
        acc = excitation[n] - state[p-1] * coeffs[p-1]

        for k in range(p-2, -1, -1):    # p-2 down to 0
            acc -= state[k] * coeffs[k]
            state[k+1] = coeffs[k] * acc + state[k]

        state[0] = acc
        output[n] = acc

    return output
```

This is NOT a standard LPC direct-form filter. It processes reflection coefficients directly through a lattice structure, avoiding the need for Levinson recursion to convert to LPC polynomial form.

---

## All Quantization Tables

### DSS SP Tables (Q15 Integer)

**FILTER_CB — Reflection Coefficient Codebook [14][32]**

Row sizes: [32, 32, 16, 16, 16, 16, 16, 16, 8, 8, 8, 8, 8, 8]

Row 0 (32 entries, 5 bits):
```
-32653 -32587 -32515 -32438 -32341 -32216 -32062 -31881
-31665 -31398 -31080 -30724 -30299 -29813 -29248 -28572
-27674 -26439 -24666 -22466 -19433 -16133 -12218  -7783
 -2834   1819   6544  11260  16050  20220  24774  28120
```

Row 1 (32 entries, 5 bits):
```
-27503 -24509 -20644 -17496 -14187 -11277  -8420  -5595
 -3013   -624   1711   3880   5844   7774   9739  11592
 13364  14903  16426  17900  19250  20586  21803  23006
 24142  25249  26275  27300  28359  29249  30118  31183
```

Rows 2-7 (16 entries each, 4 bits):
```
Row 2:  -27827 -24208 -20943 -17781 -14843 -11848  -9066  -6297
         -3660   -910   1918   5025   8223  11649  15086  18423
Row 3:  -17128 -11975  -8270  -5123  -2296    183   2503   4707
          6798   8945  11045  13239  15528  18248  21115  24785
Row 4:  -21557 -17280 -14286 -11644  -9268  -7087  -4939  -2831
          -691   1407   3536   5721   8125  10677  13721  17731
Row 5:  -15030 -10377  -7034  -4327  -1900    364   2458   4450
          6422   8374  10374  12486  14714  16997  19626  22954
Row 6:  -16155 -12362  -9698  -7460  -5258  -3359  -1547    219
          1916   3599   5299   6994   8963  11226  13716  16982
Row 7:  -14742  -9848  -6921  -4648  -2769  -1065    499   2083
          3633   5219   6857   8580  10410  12672  15561  20101
```

Rows 8-13 (8 entries each, 3 bits):
```
Row 8:  -11099  -7014  -3855  -1025   1680   4544   7807  11932
Row 9:   -9060  -4570  -1381   1419   4034   6728   9865  14149
Row 10: -12450  -7985  -4596  -1734    961   3629   6865  11142
Row 11: -11831  -7404  -4010  -1096   1606   4291   7386  11482
Row 12: -13404  -9250  -5995  -3312   -890   1594   4464   8198
Row 13: -11239  -7220  -4040  -1406    971   3321   6006   9697
```

**FIXED_CB_GAIN (6-bit, 64 entries)**
```
    0    4    8   13   17   22   26   31   35   40   44   48   53   58   63   69
   76   83   91   99  109  119  130  142  155  170  185  203  222  242  265  290
  317  346  378  414  452  494  540  591  646  706  771  843  922 1007 1101 1204
 1316 1438 1572 1719 1879 2053 2244 2453 2682 2931 3204 3502 3828 4184 4574 5000
```

**PULSE_VAL (3-bit, 8 entries)**
```
-31182 -22273 -13364  -4455   4455  13364  22273  31182
```

**ADAPTIVE_GAIN (5-bit, 32 entries)**
```
  102  231  360  488  617  746  875 1004
 1133 1261 1390 1519 1648 1777 1905 2034
 2163 2292 2421 2550 2678 2807 2936 3065
 3194 3323 3451 3580 3709 3838 3967 4096
```

**BINARY_DECREASING (15 entries)** — Powers of 2 for synthesis filter weighting:
```
32767 16384 8192 4096 2048 1024 512 256 128 64 32 16 8 4 2
```

**UNC_DECREASING (15 entries)** — Exponential decay for error correction filter:
```
32767 26214 20972 16777 13422 10737 8590 6872 5498 4398 3518 2815 2252 1801 1441
```

Ratio between consecutive elements: ~0.8 (each element is approximately 80% of the previous).

**SINC (67 entries)** — Polyphase sinc interpolation filter for 12000->11025 Hz:
```
  262   293   323   348   356   336   269   139
  -67  -358  -733 -1178 -1668 -2162 -2607 -2940
-3090 -2986 -2562 -1760  -541  1110  3187  5651
 8435 11446 14568 17670 20611 23251 25460 27125
28160 28512 28160
27125 25460 23251 20611 17670 14568 11446  8435
 5651  3187  1110  -541 -1760 -2562 -2986 -3090
-2940 -2607 -2162 -1668 -1178  -733  -358   -67
  139   269   336   356   348   323   293   262
```

The SINC table is symmetric around the center (index 34 = 28512), representing a windowed sinc function. The 11-phase polyphase structure means phases are at indices [0, 11, 22, 33, 44, 55, 66] for 6 taps each.

**COMBINATORIAL_TABLE [8][72]** — Precomputed C(n,k) values for pulse decoding:

Row 0: all zeros
Row 1: [0, 1, 2, ..., 71]  (C(n,1) = n)
Row 2: [0, 0, 1, 3, 6, 10, 15, 21, 28, ...] (C(n,2) = n*(n-1)/2)
...
Row 7: [0, 0, 0, 0, 0, 0, 0, 1, 8, 36, 120, 330, ...] (C(n,7))

**C72_BINOMIALS [8]** — C(72, k) for k=1..8:
```
72  2556  59640  1028790  13991544  156238908  1473109704  3379081753
```

### DS2 SP Tables (f64)

**SP_PITCH_GAIN (5-bit, 32 entries, DLL: VA 0x1004CF90+32)**
Linear from 0.05 to 2.0:
```
0.049805 0.112793 0.175781 0.238281 0.301270 0.364258 0.427246 0.490234
0.553223 0.615723 0.678711 0.741699 0.804688 0.867676 0.930176 0.993164
1.056152 1.119141 1.182129 1.245117 1.307617 1.370605 1.433594 1.496582
1.559570 1.622559 1.685059 1.748047 1.811035 1.874023 1.937012 2.000000
```

**SP_EXCITATION_GAIN (6-bit, 64 entries, DLL: VA 0x1004DF80+64)**
```
    0.0    4.0    8.0   13.0   17.0   22.0   26.0   31.0   35.0   40.0
   44.0   48.0   53.0   58.0   63.0   69.0   76.0   83.0   91.0   99.0
  109.0  119.0  130.0  142.0  155.0  170.0  185.0  203.0  222.0  242.0
  265.0  290.0  317.0  346.0  378.0  414.0  452.0  494.0  540.0  591.0
  646.0  706.0  771.0  843.0  922.0 1007.0 1101.0 1204.0 1316.0 1438.0
 1572.0 1719.0 1879.0 2053.0 2244.0 2453.0 2682.0 2931.0 3204.0 3502.0
 3828.0 4184.0 4574.0 5000.0
```

Note: Identical values to DSS SP's FIXED_CB_GAIN but stored as f64 instead of i32.

**SP_PULSE_AMP (3-bit, 8 entries, DLL: VA 0x1004EF70+8)**
Symmetric:
```
-0.951599 -0.679718 -0.407837 -0.135956  0.135956  0.407837  0.679718  0.951599
```

**SP Reflection Coefficient Codebooks (14 codebooks, f64)**

Codebook sizes: [32, 32, 16, 16, 16, 16, 16, 16, 8, 8, 8, 8, 8, 8]
Total entries: 204 f64 values.
Source: `ds2_lsp_codebook.npz` (extracted from DLL at VA 0x10050008, stride 256 doubles).

See source file `tables/ds2_sp.rs` for all 204 entries.

### DS2 QP Tables (f64)

**QP_PITCH_GAIN (6-bit, 64 entries, DLL: VA 0x1004BA10)**
Non-linear (different from SP!), range 0.005 to 2.0:
```
0.004913 0.056367 0.102669 0.145092 0.184286 0.220170 0.252640 0.281841
0.308202 0.332237 0.354531 0.375491 0.395460 0.414675 0.433337 0.451622
0.469648 0.487486 0.505255 0.523016 0.540824 0.558764 0.576890 0.595276
0.613963 0.632917 0.652245 0.671900 0.691902 0.712322 0.733015 0.753909
0.774967 0.796116 0.817233 0.838156 0.858900 0.879346 0.899405 0.919040
0.938366 0.957462 0.976668 0.996526 1.017693 1.041066 1.066882 1.095219
1.126158 1.159959 1.196753 1.236515 1.279487 1.325996 1.376201 1.429902
1.487140 1.548301 1.613491 1.682657 1.755914 1.833605 1.914886 1.999406
```

**QP_EXCITATION_GAIN (6-bit, 64 entries, DLL: VA 0x1004BC10)**
Non-linear, range 3.9 to 4970:
```
   3.928    7.069   10.993   16.465   23.856   32.753   42.893   54.076
  66.160   79.016   92.493  106.558  121.106  136.128  151.663  167.700
 184.251  201.424  219.212  237.740  257.014  277.134  298.164  320.054
 342.913  366.849  391.851  418.102  445.564  474.334  504.476  536.280
 569.771  604.926  642.050  681.112  722.397  766.071  812.234  861.189
 913.161  968.356 1027.220 1089.687 1156.595 1228.228 1305.279 1387.811
1476.597 1572.636 1675.856 1789.017 1911.832 2045.863 2194.195 2360.133
2545.084 2752.592 2991.921 3271.340 3603.855 4004.808 4476.587 4970.296
```

**QP_PULSE_AMP (3-bit, 8 entries, DLL: VA 0x1004BE10)**
Asymmetric (different from SP!):
```
-0.921705 -0.628998 -0.397315 -0.140886  0.206959  0.433678  0.652927  0.931249
```

Note: SP's pulse amp table is symmetric around zero; QP's is NOT — the positive values are slightly larger than the negated negative values.

**QP Reflection Coefficient Codebooks (16 codebooks, f64)**

Codebook sizes: [128, 128, 64, 64, 32, 32, 32, 32, 32, 16, 16, 16, 16, 8, 8, 8]
Total entries: 632 f64 values.
Source: `ds2_qp_codebook.npz` (extracted from AudioSDK DLL at VA 0x1004F008, stride 256 doubles).

See source file `tables/ds2_qp.rs` for all 632 entries.

---

## All Codebook Tables

The reflection coefficient codebooks are the largest data structures in the codec. They map quantization indices to reflection coefficient values.

### DS2 SP Codebook Summary

| Coeff | Bits | Entries | Range (approximate) |
|-------|------|---------|---------------------|
| 0 | 5 | 32 | -0.997 to +0.858 |
| 1 | 5 | 32 | -0.839 to +0.952 |
| 2 | 4 | 16 | -0.849 to +0.562 |
| 3 | 4 | 16 | -0.523 to +0.756 |
| 4 | 4 | 16 | -0.658 to +0.541 |
| 5 | 4 | 16 | -0.459 to +0.701 |
| 6 | 4 | 16 | -0.493 to +0.518 |
| 7 | 4 | 16 | -0.450 to +0.613 |
| 8 | 3 | 8 | -0.339 to +0.364 |
| 9 | 3 | 8 | -0.277 to +0.432 |
| 10 | 3 | 8 | -0.380 to +0.340 |
| 11 | 3 | 8 | -0.361 to +0.350 |
| 12 | 3 | 8 | -0.409 to +0.250 |
| 13 | 3 | 8 | -0.343 to +0.296 |

Coeff 13 note: The NPZ file only has 4 entries; the remaining 4 are padded from FFmpeg's row 13 / 32768.

### DS2 QP Codebook Summary

| Coeff | Bits | Entries | Range (approximate) |
|-------|------|---------|---------------------|
| 0 | 7 | 128 | -0.999 to +0.910 |
| 1 | 7 | 128 | -0.866 to +0.970 |
| 2 | 6 | 64 | -0.878 to +0.763 |
| 3 | 6 | 64 | -0.678 to +0.850 |
| 4 | 5 | 32 | -0.686 to +0.620 |
| 5 | 5 | 32 | -0.528 to +0.691 |
| 6 | 5 | 32 | -0.601 to +0.613 |
| 7 | 5 | 32 | -0.463 to +0.636 |
| 8 | 5 | 32 | -0.509 to +0.519 |
| 9 | 4 | 16 | -0.333 to +0.471 |
| 10 | 4 | 16 | -0.376 to +0.388 |
| 11 | 4 | 16 | -0.343 to +0.407 |
| 12 | 4 | 16 | -0.401 to +0.331 |
| 13 | 3 | 8 | -0.288 to +0.251 |
| 14 | 3 | 8 | -0.307 to +0.212 |
| 15 | 3 | 8 | -0.269 to +0.246 |

---

## Python Reference Decoders

### dss_decode.py (DSS SP Decoder, 723 lines)

Location: `dss_decode.py`

Verified result: **0.99999 mean correlation** vs Switch reference WAV (6092/6093 frames >0.99 correlation; frame 0 at 0.92 due to initialization warmup).

Key classes and functions:
- `BitstreamReader` — MSB-first within 16-bit LE words
- `DSSDecoder` — Main decoder class with Q15 integer arithmetic
  - `_unpack_coeffs()` — Bitstream parsing, combinatorial pulse decode, combined pitch decode
  - `_unpack_filter()` — Codebook lookup for reflection coefficients
  - `_convert_coeffs()` — Levinson recursion with overflow detection
  - `_gen_exc()` — Pitch-adaptive excitation generation
  - `_add_pulses()` — 7-pulse fixed codebook excitation
  - `_shift_sq_sub()` / `_shift_sq_add()` — LPC synthesis and error correction filters
  - `_sf_synthesis()` — Noise modulation (PRNG at coefficient 32358, energy ratio scaling)
  - `_update_state()` — 11:12 sinc interpolation resampling (12000->11025 Hz)
- `read_dss_file()` — Block-aware demuxer with empty block handling

All tables are embedded as Python lists. Integer arithmetic uses Python's arbitrary precision integers with explicit clip16/clip32767 clipping.

### ds2decode.py (DS2 SP/QP Decoder, 583 lines)

Location: `ds2decode.py`

Verified results:
- DS2 SP: **0.99999 correlation** vs Switch reference
- DS2 QP: **1.0000 correlation** (perfect match)

Key classes and functions:
- `SPBitstreamReader` — Same MSB-first within 16-bit LE words bitstream reader
- `DS2Decoder` — Unified SP/QP decoder class using f64 arithmetic
  - `_decode_sp_frames()` — SP frame loop
  - `_decode_qp_frames()` — QP frame loop with de-emphasis post-processing
  - `lattice_synthesis()` — Normalized lattice filter (shared SP/QP)
  - `decode_combined_pitch()` — Divmod pitch decoding (SP only)
  - `decode_combinatorial_index()` — C(n,k) pulse position decoding
- `read_ds2_file()` — DS2 demuxer with format detection and byte-swap for SP

Loads codebook data from external NPZ files:
- `ds2_lsp_codebook.npz` — 14 SP codebook arrays
- `ds2_qp_codebook.npz` — 16 QP codebook arrays

Quantization tables embedded as Python lists:
- `SP_PITCH_GAIN_TABLE[32]`, `SP_EXCITATION_GAIN_TABLE[64]`, `PULSE_AMP_TABLE[8]`
- `QP_PITCH_GAIN_TABLE[64]`, `QP_EXCITATION_GAIN_TABLE[64]`, `QP_PULSE_AMP_TABLE[8]`

---

## Rust Implementation

### Crate: `dss-codec`

Location: `dss-codec/`

All codebook data is embedded as `const` arrays (no runtime file dependencies). Uses `hound` for WAV output and `rubato` for arbitrary sample rate conversion.

### Module Structure

```
dss-codec/
  Cargo.toml              # clap 4, thiserror 2, hound 3, rubato 0.16
  src/
    lib.rs                 # Public API: decrypt_file(), decode_file(), decode_to_buffer(), decode_and_write()
    main.rs                # CLI binary: dss-decode
    error.rs               # DecodeError enum
    bitstream.rs           # BitstreamReader (MSB-first within 16-bit LE words)
    demux/
      mod.rs               # AudioFormat enum, detect_format()
      dss.rs               # DSS block-aware demuxer
      ds2.rs               # DS2 demuxer (SP byte-swap, QP continuous)
    codec/
      mod.rs               # Module declarations
      common.rs            # Shared: comb(), combinatorial decode, combined pitch, lattice synthesis
      dss_sp.rs            # DSS SP: Q15 integer, Levinson, sinc resample
      ds2_sp.rs            # DS2 SP: f64, lattice synthesis
      ds2_qp.rs            # DS2 QP: f64, lattice, de-emphasis
    tables/
      mod.rs
      dss_sp.rs            # COMBINATORIAL_TABLE, FILTER_CB, SINC, etc.
      ds2_sp.rs            # 14 SP codebook arrays
      ds2_qp.rs            # 16 QP codebook arrays
      ds2_quant.rs         # Pitch/excitation/pulse gain tables (SP and QP)
    output/
      mod.rs               # OutputConfig
      wav.rs               # WAV writer (16/24/32-bit)
      resample.rs          # Rubato FFT resampler
  tests/
    integration.rs         # Format detection and basic decode tests
```

### Public Library API

```rust
use dss_codec::{
    decode_file,
    decode_file_with_password,
    decode_to_buffer,
    decode_to_buffer_with_password,
    decode_and_write,
    decrypt_file,
    decrypt_to_bytes,
    AudioBuffer,
};
use dss_codec::demux::{detect_format, AudioFormat};
use dss_codec::output::OutputConfig;

// Decode from file path
let buf: AudioBuffer = decode_file(Path::new("recording.ds2"))?;
// buf.samples: Vec<f64>, buf.native_rate: u32, buf.format: AudioFormat

// Decode from bytes
let data = std::fs::read("recording.dss")?;
let buf = decode_to_buffer(&data)?;

// Decode encrypted DS2 with a password
let encrypted = std::fs::read("encrypted.ds2")?;
let buf = decode_to_buffer_with_password(&encrypted, Some(b"1234"))?;

// Normalize to plain container bytes (plain input passes through unchanged)
let plain_ds2 = decrypt_file(Path::new("encrypted.ds2"), Some(b"1234"))?;
let plain_bytes = decrypt_to_bytes(&encrypted, Some(b"1234"))?;

// Decode and write WAV
let config = OutputConfig { sample_rate: Some(16000), bit_depth: 16, channels: 1 };
decode_and_write(Path::new("in.ds2"), Path::new("out.wav"), &config)?;
```

### CLI

```
dss-decode [OPTIONS] <INPUT...>

  Options:
    -O, --output-file <PATH>   Output file (single input mode)
    -f, --format <FORMAT>      Output format [default: wav]
    -r, --rate <HZ>            Output sample rate [default: native]
    -b, --bits <16|24|32>      Bit depth [default: 16]
    -c, --channels <1|2>       Channels [default: 1]
    -o, --output-dir <DIR>     Batch output directory
    -q, --quiet                Suppress status output
        --decrypt              Save decrypted/plain container bytes instead of WAV
        --password <PASSWORD>  Password for encrypted DS2 input
        --info                 Print file metadata only
  ```

---

## Verification Results

### Rust vs Python Reference

| Decoder | Correlation | Exact Match | Notes |
|---------|-------------|-------------|-------|
| DSS SP | 1.0000 | 100% | Bit-exact (pure integer arithmetic) |
| DS2 SP | 1.0000 | 99% | +-1 rounding (f64 -> i16 boundary) |
| DS2 QP | 1.0000 | 100% | Bit-exact |

### Rust vs Olympus DirectShow Reference WAV

| Decoder | Correlation | Notes |
|---------|-------------|-------|
| DS2 QP | 0.99999997 | Near-perfect |
| DS2 SP | 0.995 | Slight difference (DirectShow may use post-filter) |
| DSS SP | 0.99999 | All frames except frame 0 (warmup) |

### Performance

~140x faster than Python: 45ms vs 6.3s for 146 seconds of DSS SP audio (6093 frames).

---

## DLL Function Map

### DssDecoder.dll / AudioSDK DLL

| VA | Function | Purpose |
|----|----------|---------|
| 0x100023B0 | Mode selector | Sample rate selection: SP=12000, LP=8000, QP=16000 |
| 0x10011560 | QP subframe synthesis | Per-subframe pitch prediction + excitation + pitch memory |
| 0x10011A90 | QP frame decode | Calls 0x10011560 per subframe |
| 0x10011EB0 | QP init | Initialize QP decoder state |
| 0x10012350 | SP synthesis pipeline | SP frame synthesis |
| 0x10012CE0 | SP subframe synthesis | SP per-subframe processing |
| 0x10013380 | QP codebook lookup | Base 0x1004F008, stride 256 doubles |
| 0x10013550 | QP lattice filter | Identical to 0x10014230 |
| 0x10014230 | SP lattice filter | Normalized lattice synthesis |
| 0x10015950 | DssSpDec init | Initialize SP decoder state |
| 0x100167D0 | Data loader | Load 16-bit LE words |
| 0x10016C50 | C(n,k) | Binomial coefficient computation |
| 0x100175B0 | Synthesis filter | ~800 bytes |
| 0x10017460 | Bitstream reader | 1-bit-at-a-time MSB-first within 16-bit LE words |
| 0x100179D0 | QP base init | min_pitch, max_pitch, num_coeffs |
| 0x10017A80 | QP full init | Alloc table, gain bits, etc. |
| 0x10017E70 | QP decode pipeline | Dequant -> excitation -> filter per subframe |
| 0x100180C0 | SP init | All SP codec parameters |
| 0x100182D0 | Total bits calc | Frame bit budget calculator |
| 0x10018450 | SP decode | Main SP frame decoder |
| 0x10018800 | Excitation gen | Pitch + combinatorial codebook |
| 0x10018E90 | SP LSP dequant | Codebook base 0x10050008, stride 256 |
| 0x10019060 | SP synthesis filter | Lattice (identical to QP's 0x10019D40) |
| 0x10019C20 | QP LSP dequant | Codebook base 0x10058890, stride 510 |
| 0x10019D40 | QP synthesis filter | Lattice |
| 0x1001A2F0 | Frame unpacking | 2031 bytes, bit field extraction |
| 0x10036340 | ftol | Float to int truncation |

### Data Addresses

| VA | Description |
|----|-------------|
| 0x1004BA10 | QP pitch gain table (64 doubles) |
| 0x1004BC10 | QP excitation gain table (64 doubles) |
| 0x1004BE10 | QP pulse amplitude table (8 doubles) |
| 0x1004CF90 | SP pitch gain table (32 doubles, offset +32) |
| 0x1004DF80 | SP excitation gain table (64 doubles, offset +64) |
| 0x1004EF70 | SP pulse amplitude table (8 doubles, offset +8) |
| 0x1004F008 | QP codebook base (16 coeffs, stride 256 doubles) |
| 0x10050008 | SP codebook base (14 coeffs, stride 256 doubles) |
| 0x10058890 | QP codebook (alternate, stride 510 doubles) |
| 0x10065988 | De-emphasis alpha = 0.1 |
| 0x10066970 | Constant: 6.0 (subframe divisor) |

### DssParser.dll Functions

| VA | Function | Purpose |
|----|----------|---------|
| 0x10009270 | Bit position advancement | Navigate bitstream |
| 0x10009700 | Block loading | Header parsing |
| 0x10009890 | Frame size reader | Lookup table at 0x1002CE20 |
| 0x10009980 | Frame iteration | Block processing loop |
