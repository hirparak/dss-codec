//! G.723.1 speech decoder (ITU-T Recommendation G.723.1 with Annex A), the
//! codec of DSS and DS2 LP blocks.
//!
//! Decodes both rates (6.3 kbit/s MP-MLQ and 5.3 kbit/s ACELP), SID frames
//! and untransmitted frames with comfort noise, and erased frames with
//! concealment. Output is 240 samples per 30 ms frame at 8000 Hz.
//!
//! The arithmetic is the Recommendation's 16- and 32-bit fixed point, step
//! for step: every product, shift and sum saturates or floors exactly where
//! the reference decoder does, so the output matches the ITU test vectors
//! sample for sample. The helpers at the top of the file are those
//! primitives; their names say their width (`add16`, `mac32`, ...).

use crate::tables::g723_1::{
    ACB_GAIN_170, ACB_GAIN_85, ACELP_PITCH_GAIN, ACELP_PITCH_LAG_OFFSET, CNG_GAIN_SEGMENT_BASE,
    CNG_GAIN_SEGMENT_BOUNDS, COSINE, FIXED_GAIN, LSP_DC, LSP_VQ_BAND0, LSP_VQ_BAND1, LSP_VQ_BAND2,
    MPMLQ_CODEBOOK_SIZE, MPMLQ_COMBINATORIAL, MPMLQ_PULSE_COUNT, NORM_MULTIPLIER,
    PITCH_POSTFILTER_WEIGHT, POSTFILTER_POLE_WEIGHTS, POSTFILTER_ZERO_WEIGHTS,
};

/// Samples per frame.
pub const FRAME_SAMPLES: usize = 240;
/// Output sample rate in Hz.
pub const SAMPLE_RATE: u32 = 8000;

const SUBFRAME: usize = 60;
const ORDER: usize = 10;
/// Excitation history: the longest pitch lag, 18 + 127.
const HISTORY: usize = 145;
const LAG_MIN: i16 = 18;
/// Largest lag the classifier and the pitch postfilter search around.
const LAG_SEARCH_MAX: i16 = 142;
/// Below this even lag, 6.3 kbit/s gains use the 85-entry book and may carry
/// a pulse train bit.
const SHORT_LAG: i16 = 58;
const CNG_SEED: i16 = 12345;

/// Size in bytes of a frame whose first byte is `first_byte`, from the frame
/// type in its two low bits: 24 for 6.3 kbit/s, 20 for 5.3 kbit/s, 4 for a
/// SID frame and 1 for an untransmitted frame.
pub fn frame_size(first_byte: u8) -> usize {
    match first_byte & 3 {
        0 => 24,
        1 => 20,
        2 => 4,
        _ => 1,
    }
}

// --- Fixed-point primitives ------------------------------------------------

fn sat16(v: i32) -> i16 {
    v.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

fn sat32(v: i64) -> i32 {
    v.clamp(i32::MIN.into(), i32::MAX.into()) as i32
}

fn add16(a: i16, b: i16) -> i16 {
    sat16(i32::from(a) + i32::from(b))
}

fn sub16(a: i16, b: i16) -> i16 {
    sat16(i32::from(a) - i32::from(b))
}

fn neg16(a: i16) -> i16 {
    a.checked_neg().unwrap_or(i16::MAX)
}

fn abs16(a: i16) -> i16 {
    a.checked_abs().unwrap_or(i16::MAX)
}

/// a * b / 2^15, floored.
fn mul16(a: i16, b: i16) -> i16 {
    sat16((i32::from(a) * i32::from(b)) >> 15)
}

/// a * b / 2^15, rounded.
fn mul16r(a: i16, b: i16) -> i16 {
    sat16((i32::from(a) * i32::from(b) + 0x4000) >> 15)
}

fn shl16(a: i16, n: i16) -> i16 {
    if n < 0 {
        return shr16(a, n.saturating_neg());
    }
    if n > 15 {
        return match a {
            0 => 0,
            a if a > 0 => i16::MAX,
            _ => i16::MIN,
        };
    }
    sat16(i32::from(a) << n)
}

fn shr16(a: i16, n: i16) -> i16 {
    if n < 0 {
        return shl16(a, n.saturating_neg());
    }
    if n >= 15 {
        return if a < 0 { -1 } else { 0 };
    }
    a >> n
}

/// Left shifts that bring a non-zero `a` to the range of 2^14 to 2^15.
fn norm16(a: i16) -> i16 {
    match a {
        0 => 0,
        -1 => 15,
        _ => {
            let b = if a < 0 { !a } else { a };
            b.leading_zeros() as i16 - 1
        }
    }
}

/// a * 2^15 / b for 0 <= a <= b, floored.
fn div16(a: i16, b: i16) -> i16 {
    debug_assert!(0 <= a && a <= b && b > 0);
    if a == 0 {
        0
    } else if a == b {
        i16::MAX
    } else {
        ((i32::from(a) << 15) / i32::from(b)) as i16
    }
}

fn lmul(a: i16, b: i16) -> i32 {
    if a == i16::MIN && b == i16::MIN {
        i32::MAX
    } else {
        2 * i32::from(a) * i32::from(b)
    }
}

fn add32(x: i32, y: i32) -> i32 {
    x.saturating_add(y)
}

fn sub32(x: i32, y: i32) -> i32 {
    x.saturating_sub(y)
}

fn mac32(x: i32, a: i16, b: i16) -> i32 {
    add32(x, lmul(a, b))
}

fn msu32(x: i32, a: i16, b: i16) -> i32 {
    sub32(x, lmul(a, b))
}

fn neg32(x: i32) -> i32 {
    x.checked_neg().unwrap_or(i32::MAX)
}

fn abs32(x: i32) -> i32 {
    x.checked_abs().unwrap_or(i32::MAX)
}

fn shl32(x: i32, n: i16) -> i32 {
    if n <= 0 {
        return shr32(x, n.saturating_neg());
    }
    if n >= 32 {
        return match x {
            0 => 0,
            x if x > 0 => i32::MAX,
            _ => i32::MIN,
        };
    }
    sat32(i64::from(x) << n)
}

fn shr32(x: i32, n: i16) -> i32 {
    if n < 0 {
        return shl32(x, n.saturating_neg());
    }
    if n >= 31 {
        return if x < 0 { -1 } else { 0 };
    }
    x >> n
}

fn hi16(x: i32) -> i16 {
    (x >> 16) as i16
}

fn lo16(x: i32) -> i16 {
    x as i16
}

fn dep_hi(a: i16) -> i32 {
    i32::from(a) << 16
}

fn round16(x: i32) -> i16 {
    hi16(add32(x, 0x8000))
}

/// Left shifts that bring a non-zero `x` to the range of 2^30 to 2^31.
fn norm32(x: i32) -> i16 {
    match x {
        0 => 0,
        -1 => 31,
        _ => {
            let b = if x < 0 { !x } else { x };
            b.leading_zeros() as i16 - 1
        }
    }
}

/// About x * v / 2^15: the unsigned low half and the signed high half of x
/// are multiplied apart, which the reference does and a plain product does
/// not reproduce.
fn mul32x16(x: i32, v: i16) -> i32 {
    let lo = x & 0xffff;
    let t = (lo * i32::from(v)) >> 15;
    mac32(t, v, hi16(x))
}

/// n / (2 * d) for n >= 0 and d > 0, at most 32767.
fn div32x16(n: i32, d: i16) -> i16 {
    debug_assert!(n >= 0 && d > 0);
    if n >= dep_hi(d) {
        i16::MAX
    } else {
        (n / (2 * i32::from(d))) as i16
    }
}

/// Greatest r with bit 0 clear and 2 * r * r <= n, its bits chosen from the
/// top.
fn isqrt(n: i32) -> i16 {
    let mut r: i16 = 0;
    for k in (1..=14).rev() {
        let t = add16(r, 1 << k);
        if n >= lmul(t, t) {
            r = t;
        }
    }
    r
}

fn rand16(seed: &mut i16) -> i16 {
    let w = i32::from(*seed as u16) * 521 + 259;
    *seed = lo16(w);
    *seed
}

/// Scales `v` in place so its largest magnitude reaches 2^11 to 2^12 and
/// returns the shift that was applied, minus 3.
fn vnorm(v: &mut [i16]) -> i16 {
    let max = v.iter().fold(0, |m, &x| m.max(abs16(x)));
    let z = norm16(max);
    let multiplier = NORM_MULTIPLIER[z as usize];
    for x in v.iter_mut() {
        *x = lo16(shr32(lmul(multiplier, *x), 4));
    }
    z - 3
}

/// Sum of a[j] * b[j] with saturation at every step.
fn dot(a: &[i16], b: &[i16]) -> i32 {
    a.iter().zip(b).fold(0, |acc, (&x, &y)| mac32(acc, x, y))
}

// --- Bitstream -------------------------------------------------------------

/// Reads fields from a frame, each from its least significant bit up. Bits
/// past the end of the frame read as 0.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, width: usize) -> i32 {
        let mut value = 0;
        for t in 0..width {
            let bit = self
                .data
                .get(self.pos / 8)
                .map_or(0, |byte| (byte >> (self.pos % 8)) & 1);
            value |= i32::from(bit) << t;
            self.pos += 1;
        }
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rate {
    High,
    Low,
}

/// Parameters of a good active frame.
#[derive(Debug, PartialEq, Eq)]
struct Active {
    rate: Rate,
    lsp: i32,
    /// Even lag of subframes 0 and 1, then of subframes 2 and 3.
    lag: [i16; 2],
    /// Lag delta of each subframe; 1 for the even subframes.
    delta: [i16; 4],
    /// Adaptive codebook gain index.
    acb_gain: [usize; 4],
    /// Fixed codebook gain index.
    fcb_gain: [usize; 4],
    train: [bool; 4],
    grid: [usize; 4],
    pos: [i32; 4],
    sign: [i32; 4],
}

impl Active {
    fn even_lag(&self, subframe: usize) -> i16 {
        self.lag[subframe / 2]
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Active(Active),
    Sid {
        lsp: i32,
        gain: i16,
    },
    Untransmitted,
    /// Erased, or an active frame with a forbidden lag or gain code.
    Bad,
}

fn parse(data: &[u8]) -> Frame {
    let mut bits = BitReader { data, pos: 0 };
    let rate = match bits.read(2) {
        0 => Rate::High,
        1 => Rate::Low,
        2 => {
            let lsp = bits.read(24);
            let gain = bits.read(6) as i16;
            return Frame::Sid { lsp, gain };
        }
        _ => return Frame::Untransmitted,
    };
    let lsp = bits.read(24);

    let code = bits.read(7);
    if code > 123 {
        return Frame::Bad;
    }
    let lag0 = code as i16 + LAG_MIN;
    let delta1 = bits.read(2) as i16;
    let code = bits.read(7);
    if code > 123 {
        return Frame::Bad;
    }
    let lag2 = code as i16 + LAG_MIN;
    let delta3 = bits.read(2) as i16;

    let mut frame = Active {
        rate,
        lsp,
        lag: [lag0, lag2],
        delta: [1, delta1, 1, delta3],
        acb_gain: [0; 4],
        fcb_gain: [0; 4],
        train: [false; 4],
        grid: [0; 4],
        pos: [0; 4],
        sign: [0; 4],
    };

    for i in 0..4 {
        let mut gain = bits.read(12);
        let mut bound = 170;
        if rate == Rate::High && frame.even_lag(i) < SHORT_LAG {
            frame.train[i] = gain >> 11 != 0;
            gain &= 0x7ff;
            bound = 85;
        }
        frame.acb_gain[i] = (gain / 24) as usize;
        frame.fcb_gain[i] = (gain % 24) as usize;
        if frame.acb_gain[i] >= bound {
            return Frame::Bad;
        }
    }
    for grid in &mut frame.grid {
        *grid = bits.read(1) as usize;
    }

    if rate == Rate::High {
        bits.read(1);
        let msb = bits.read(13);
        let (q, s) = (msb / 90, msb % 90);
        let high = [q / 9, q % 9, s / 9, s % 9];
        for (i, &width) in [16, 14, 16, 14].iter().enumerate() {
            frame.pos[i] = (high[i] << width) + bits.read(width);
        }
        for (i, width) in [6, 5, 6, 5].into_iter().enumerate() {
            frame.sign[i] = bits.read(width);
        }
    } else {
        for pos in &mut frame.pos {
            *pos = bits.read(12);
        }
        for sign in &mut frame.sign {
            *sign = bits.read(4);
        }
    }
    Frame::Active(frame)
}

// --- LSP decoding and LPC conversion ----------------------------------------

/// Inverse-quantizes the LSP index against the previous vector. A bad frame
/// uses index 0 with a stronger predictor and a wider spacing.
fn decode_lsp(index: i32, prev: &[i16; ORDER], bad: bool) -> [i16; ORDER] {
    let (index, predictor, spacing) = if bad {
        (0, 23552, 512)
    } else {
        (index, 12288, 256)
    };
    let band0 = &LSP_VQ_BAND0[((index >> 16) & 0xff) as usize];
    let band1 = &LSP_VQ_BAND1[((index >> 8) & 0xff) as usize];
    let band2 = &LSP_VQ_BAND2[(index & 0xff) as usize];
    let mut lsp = [0i16; ORDER];
    lsp[..3].copy_from_slice(band0);
    lsp[3..6].copy_from_slice(band1);
    lsp[6..].copy_from_slice(band2);

    for j in 0..ORDER {
        let d = sub16(prev[j], LSP_DC[j]);
        lsp[j] = add16(lsp[j], mul16r(d, predictor));
        lsp[j] = add16(lsp[j], LSP_DC[j]);
    }

    for _ in 0..10 {
        lsp[0] = lsp[0].max(0x180);
        lsp[ORDER - 1] = lsp[ORDER - 1].min(0x7e00);
        for j in 1..ORDER {
            let t = sub16(add16(spacing, lsp[j - 1]), lsp[j]);
            if t > 0 {
                let t = shr16(t, 1);
                lsp[j - 1] = sub16(lsp[j - 1], t);
                lsp[j] = add16(lsp[j], t);
            }
        }
        let unstable = (1..ORDER).any(|j| sub16(sub16(add16(lsp[j - 1], spacing), 4), lsp[j]) > 0);
        if !unstable {
            return lsp;
        }
    }
    *prev
}

/// One step of the LSP to LPC polynomial recursion, for P or for Q.
fn polynomial_step(p: &mut [i32; 6], x: i16, m: usize) {
    p[m + 1] = add32(mul32x16(p[m], x), p[m - 1]);
    for j in (2..=m).rev() {
        p[j] = add32(
            add32(mul32x16(p[j - 1], x), shr32(p[j], 1)),
            shr32(p[j - 2], 1),
        );
    }
    p[0] = shr32(p[0], 1);
    p[1] = shr32(add32(shr32(dep_hi(x), m as i16), p[1]), 1);
}

/// Entry `j mod 512` of the cosine table. The table holds one period, so
/// this continues the cosine past both ends.
fn cosine(j: i16) -> i16 {
    COSINE[(j & 0x1ff) as usize]
}

/// About -2 cos of the LSP `q`, by linear interpolation in the cosine table.
/// The stability step can leave an LSP below zero; the table is then read
/// periodically, as cos(-x) = cos(x).
fn lsp_cosine(q: i16) -> i16 {
    let j = shr16(q, 7);
    let delta = sub16(cosine(j + 1), cosine(j));
    let fraction = add16(shl16(q & 0x7f, 8), 0x80);
    let acc = shl32(mac32(dep_hi(cosine(j)), delta, fraction), 1);
    neg16(round16(acc))
}

fn lsp_to_lpc(lsp: &[i16; ORDER]) -> [i16; ORDER] {
    let c = lsp.map(lsp_cosine);

    let start = |a: i16, b: i16| {
        [
            0x1000_0000,
            mac32(lmul(a, 0x2000), b, 0x2000),
            add32(shr32(lmul(a, b), 1), 0x2000_0000),
            0,
            0,
            0,
        ]
    };
    let mut p = start(c[0], c[2]);
    let mut q = start(c[1], c[3]);
    for m in 2..=4 {
        polynomial_step(&mut p, c[2 * m], m);
        polynomial_step(&mut q, c[2 * m + 1], m);
    }

    let mut a = [0i16; ORDER];
    for k in 0..5 {
        let sum = add32(p[k], p[k + 1]);
        a[k] = neg16(round16(shl32(add32(sub32(sum, q[k]), q[k + 1]), 3)));
        a[ORDER - 1 - k] = neg16(round16(shl32(sub32(add32(sum, q[k]), q[k + 1]), 3)));
    }
    a
}

/// LPC coefficients of the four subframes, from LSPs interpolated between the
/// previous frame's vector and this frame's.
fn subframe_lpc(current: &[i16; ORDER], prev: &[i16; ORDER]) -> [[i16; ORDER]; 4] {
    let mut lpc = [[0i16; ORDER]; 4];
    for (i, a) in lpc.iter_mut().enumerate() {
        let w = (-8192 * (i as i32 + 1)) as i16;
        let mut q = [0i16; ORDER];
        for j in 0..ORDER {
            let acc = msu32(mac32(dep_hi(prev[j]), w, prev[j]), w, current[j]);
            q[j] = round16(acc);
        }
        *a = lsp_to_lpc(&q);
    }
    lpc
}

// --- Excitation ----------------------------------------------------------------

/// Adaptive codebook contribution of the subframe starting at `start` in
/// `exc`, from the 145 samples before it.
fn adaptive_vector(
    exc: &[i16],
    start: usize,
    even_lag: i16,
    delta: i16,
    gain: usize,
    short_book: bool,
) -> [i16; SUBFRAME] {
    let lag = (even_lag - 1 + delta) as usize;
    let mut delayed = [0i16; SUBFRAME + 4];
    delayed[0] = exc[start - lag - 2];
    delayed[1] = exc[start - lag - 1];
    for k in 0..SUBFRAME + 2 {
        delayed[2 + k] = exc[start - lag + k % lag];
    }
    let taps = if short_book {
        &ACB_GAIN_85[gain]
    } else {
        &ACB_GAIN_170[gain]
    };
    let mut u = [0i16; SUBFRAME];
    for (n, out) in u.iter_mut().enumerate() {
        *out = round16(shl32(dot(&delayed[n..n + 5], taps), 1));
    }
    u
}

/// 6.3 kbit/s fixed codebook vector: 5 or 6 pulses on one grid, then a
/// pulse train at the lag when the gain code asks for it.
fn mpmlq_vector(frame: &Active, i: usize) -> [i16; SUBFRAME] {
    let mut v = [0i16; SUBFRAME];
    if frame.pos[i] >= MPMLQ_CODEBOOK_SIZE[i] {
        return v;
    }
    let gain = FIXED_GAIN[frame.fcb_gain[i]];
    let mut r = 6 - MPMLQ_PULSE_COUNT[i];
    let mut code = frame.pos[i];
    for c in 0..30 {
        let rest = sub32(code, MPMLQ_COMBINATORIAL[r][c]);
        if rest >= 0 {
            code = rest;
            continue;
        }
        r += 1;
        let negative = (frame.sign[i] >> (6 - r)) & 1 != 0;
        v[frame.grid[i] + 2 * c] = if negative { neg16(gain) } else { gain };
        if r == 6 {
            break;
        }
    }

    if frame.train[i] {
        let lag = frame.even_lag(i) as usize;
        let pulses = v;
        let mut shift = lag;
        while shift < SUBFRAME {
            for n in shift..SUBFRAME {
                v[n] = add16(v[n], pulses[n - shift]);
            }
            shift += lag;
        }
    }
    v
}

/// 5.3 kbit/s fixed codebook vector: 4 pulses on one grid, then recursive
/// pitch sharpening.
fn acelp_vector(frame: &Active, i: usize) -> [i16; SUBFRAME] {
    let mut w = [0i16; SUBFRAME + 4];
    let gain = FIXED_GAIN[frame.fcb_gain[i]];
    for k in 0..4 {
        let slot = ((frame.pos[i] >> (3 * k)) & 7) as usize;
        let positive = (frame.sign[i] >> k) & 1 != 0;
        w[8 * slot + frame.grid[i] + 2 * k] = if positive { gain } else { neg16(gain) };
    }
    let mut v = [0i16; SUBFRAME];
    v.copy_from_slice(&w[..SUBFRAME]);

    let acb = frame.acb_gain[i];
    let lag = frame.even_lag(i) - 1 + frame.delta[i] + ACELP_PITCH_LAG_OFFSET[acb];
    if lag < SHORT_LAG {
        let lag = lag as usize;
        for n in lag..SUBFRAME {
            v[n] = add16(v[n], mul16(v[n - lag], ACELP_PITCH_GAIN[acb]));
        }
    }
    v
}

/// Pitch postfilter of one subframe: lag, gain of the delayed signal and
/// gain of the current one.
struct PitchPostfilter {
    lag: isize,
    delayed_gain: i16,
    gain: i16,
}

impl PitchPostfilter {
    const OFF: Self = Self {
        lag: 0,
        delayed_gain: 0,
        gain: i16::MAX,
    };

    /// Searches the normalised excitation `xn` around `even_lag` backward
    /// and forward from the subframe starting at `b`.
    fn search(xn: &[i16; HISTORY + FRAME_SAMPLES], b: usize, even_lag: i16, rate: Rate) -> Self {
        let lag = even_lag.min(LAG_SEARCH_MAX) as usize;
        let current = &xn[b..b + SUBFRAME];
        let subframe_end = b - HISTORY + SUBFRAME;

        let (mut back_lag, mut back_corr) = (0usize, 0);
        let (mut fwd_lag, mut fwd_corr) = (0usize, 0);
        for t in lag - 3..=lag + 3 {
            let c = dot(current, &xn[b - t..b - t + SUBFRAME]);
            if c > back_corr {
                back_corr = c;
                back_lag = t;
            }
        }
        for t in lag - 3..=lag + 3 {
            let c = if subframe_end + t <= FRAME_SAMPLES {
                dot(current, &xn[b + t..b + t + SUBFRAME])
            } else {
                0
            };
            if c > fwd_corr {
                fwd_corr = c;
                fwd_lag = t;
            }
        }
        if back_lag == 0 && fwd_lag == 0 {
            return Self::OFF;
        }

        let mut v = [0i32; 5];
        v[0] = dot(current, current);
        if back_lag != 0 {
            let past = &xn[b - back_lag..b - back_lag + SUBFRAME];
            v[1] = dot(current, past);
            v[2] = dot(past, past);
        }
        if fwd_lag != 0 {
            let ahead = &xn[b + fwd_lag..b + fwd_lag + SUBFRAME];
            v[3] = dot(current, ahead);
            v[4] = dot(ahead, ahead);
        }
        let max = v.iter().fold(0, |m, &x| m.max(x));
        let shift = norm32(max);
        let w = v.map(|x| hi16(shl32(x, shift)));

        let backward = if back_lag == 0 {
            false
        } else if fwd_lag == 0 {
            true
        } else {
            let a = lmul(mul16r(w[1], w[1]), w[4]);
            let c = lmul(mul16r(w[3], w[3]), w[2]);
            a > c
        };
        let (lag, corr, energy) = if backward {
            (-(back_lag as isize), w[1], w[2])
        } else {
            (fwd_lag as isize, w[3], w[4])
        };
        Self::gains(lag, w[0], corr, energy, rate)
    }

    fn gains(lag: isize, ten: i16, corr: i16, energy: i16, rate: Rate) -> Self {
        let a = shr32(lmul(ten, energy), 2);
        let b = lmul(corr, corr);
        if b <= a {
            return Self { lag, ..Self::OFF };
        }
        let weight = match rate {
            Rate::High => PITCH_POSTFILTER_WEIGHT[0],
            Rate::Low => PITCH_POSTFILTER_WEIGHT[1],
        };
        let g = if corr >= energy {
            weight
        } else {
            mul16(div16(corr, energy), weight)
        };
        let mut acc = shr32(dep_hi(ten), 1);
        acc = mac32(acc, corr, g);
        acc = add32(acc, shr32(lmul(energy, mul16(g, g)), 1));
        let den = round16(acc);
        let num = shr32(dep_hi(ten), 1);
        let q = if num >= dep_hi(den) {
            i16::MAX
        } else {
            div32x16(num, den)
        };
        let gain = isqrt(dep_hi(q));
        Self {
            lag,
            delayed_gain: mul16(g, gain),
            gain,
        }
    }
}

// --- Decoder -------------------------------------------------------------------

/// Frame kind as the comfort noise logic tracks it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Untransmitted,
    Active,
    Sid,
}

/// G.723.1 decoder state. One decoder decodes one stream of frames in order.
pub struct G7231Decoder {
    postfilter: bool,
    prev_lsp: [i16; ORDER],
    prev_exc: [i16; HISTORY],
    synth_mem: [i16; ORDER],
    pf_fir_mem: [i16; ORDER],
    pf_iir_mem: [i16; ORDER],
    tilt: i16,
    out_gain: i16,
    erase_count: i16,
    conceal_gain: i16,
    conceal_lag: i16,
    conceal_seed: i16,
    past_kind: Kind,
    /// SID gain during comfort noise; after an active frame, the energy of
    /// its last 120 excitation samples.
    sid_gain: i16,
    /// Smoothed comfort noise gain; after an active frame, the shift of the
    /// energy in `sid_gain`.
    cur_gain: i16,
    sid_lsp: [i16; ORDER],
    cng_seed: i16,
}

impl Default for G7231Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G7231Decoder {
    /// A decoder with the postfilter on, as the Recommendation's decoder
    /// runs by default.
    pub fn new() -> Self {
        Self::with_postfilter(true)
    }

    /// A decoder with the postfilter on or off.
    pub fn with_postfilter(postfilter: bool) -> Self {
        Self {
            postfilter,
            prev_lsp: LSP_DC,
            prev_exc: [0; HISTORY],
            synth_mem: [0; ORDER],
            pf_fir_mem: [0; ORDER],
            pf_iir_mem: [0; ORDER],
            tilt: 0,
            out_gain: 0x1000,
            erase_count: 0,
            conceal_gain: 0,
            conceal_lag: 0,
            conceal_seed: 0,
            past_kind: Kind::Active,
            sid_gain: 0,
            cur_gain: 0,
            sid_lsp: LSP_DC,
            cng_seed: CNG_SEED,
        }
    }

    /// Decodes one frame. Its size is `frame_size(frame[0])`; bytes past the
    /// end of `frame` read as 0, and an empty frame decodes as erased.
    pub fn decode_frame(&mut self, frame: &[u8]) -> [i16; FRAME_SAMPLES] {
        if frame.is_empty() {
            return self.decode(Frame::Bad);
        }
        self.decode(parse(frame))
    }

    /// Conceals one frame that was lost or arrived damaged.
    pub fn decode_erased(&mut self) -> [i16; FRAME_SAMPLES] {
        self.decode(Frame::Bad)
    }

    fn decode(&mut self, frame: Frame) -> [i16; FRAME_SAMPLES] {
        let (exc, lpc, kind) = match frame {
            Frame::Active(active) => {
                let (exc, lpc) = self.active(Some(&active));
                (exc, lpc, Kind::Active)
            }
            Frame::Bad if self.past_kind == Kind::Active => {
                let (exc, lpc) = self.active(None);
                (exc, lpc, Kind::Active)
            }
            Frame::Bad | Frame::Untransmitted => {
                let (exc, lpc) = self.comfort_noise(None);
                (exc, lpc, Kind::Untransmitted)
            }
            Frame::Sid { lsp, gain } => {
                let (exc, lpc) = self.comfort_noise(Some((lsp, gain)));
                (exc, lpc, Kind::Sid)
            }
        };
        self.past_kind = kind;

        let mut out = [0i16; FRAME_SAMPLES];
        for (i, a) in lpc.iter().enumerate() {
            let range = i * SUBFRAME..(i + 1) * SUBFRAME;
            let mut y = self.synthesize(&exc[range.clone()], a);
            if self.postfilter {
                let energy = self.formant_postfilter(&mut y, a);
                self.scale_gain(&mut y, energy);
            }
            out[range].copy_from_slice(&y);
        }
        out
    }

    /// Excitation and LPC of an active frame; `None` conceals an erased one.
    fn active(&mut self, frame: Option<&Active>) -> ([i16; FRAME_SAMPLES], [[i16; ORDER]; 4]) {
        self.erase_count = if frame.is_some() {
            0
        } else {
            add16(self.erase_count, 1).min(3)
        };
        let lsp = decode_lsp(frame.map_or(0, |f| f.lsp), &self.prev_lsp, frame.is_none());
        let lpc = subframe_lpc(&lsp, &self.prev_lsp);
        self.prev_lsp = lsp;

        self.conceal_gain = match frame {
            Some(f) if self.erase_count == 0 => {
                FIXED_GAIN[shr16(add16(f.fcb_gain[2] as i16, f.fcb_gain[3] as i16), 1) as usize]
            }
            _ => mul16r(self.conceal_gain, 0x6000),
        };

        let exc = match frame {
            Some(f) => self.good_excitation(f),
            None => self.concealed_excitation(),
        };
        self.cng_seed = CNG_SEED;
        (exc, lpc)
    }

    fn good_excitation(&mut self, frame: &Active) -> [i16; FRAME_SAMPLES] {
        let mut x = [0i16; HISTORY + FRAME_SAMPLES];
        x[..HISTORY].copy_from_slice(&self.prev_exc);
        for i in 0..4 {
            let fixed = match frame.rate {
                Rate::High => mpmlq_vector(frame, i),
                Rate::Low => acelp_vector(frame, i),
            };
            let start = HISTORY + i * SUBFRAME;
            let even_lag = frame.even_lag(i);
            let short_book = frame.rate == Rate::High && even_lag < SHORT_LAG;
            let adaptive = adaptive_vector(
                &x,
                start,
                even_lag,
                frame.delta[i],
                frame.acb_gain[i],
                short_book,
            );
            for n in 0..SUBFRAME {
                x[start + n] = add16(shl16(fixed[n], 1), adaptive[n]);
            }
        }

        let mut xn = x;
        self.cur_gain = vnorm(&mut xn);
        self.classify(&xn, frame.lag[1]);

        let mut exc = [0i16; FRAME_SAMPLES];
        exc.copy_from_slice(&x[HISTORY..]);
        if self.postfilter {
            let filters: [PitchPostfilter; 4] = std::array::from_fn(|i| {
                PitchPostfilter::search(&xn, HISTORY + i * SUBFRAME, frame.even_lag(i), frame.rate)
            });
            for (i, filter) in filters.iter().enumerate() {
                let b = HISTORY + i * SUBFRAME;
                let delayed = (b as isize + filter.lag) as usize;
                for n in 0..SUBFRAME {
                    let acc = mac32(
                        lmul(x[b + n], filter.gain),
                        x[delayed + n],
                        filter.delayed_gain,
                    );
                    exc[i * SUBFRAME + n] = round16(acc);
                }
            }
        }

        self.sid_lsp = self.prev_lsp;
        self.prev_exc.copy_from_slice(&x[FRAME_SAMPLES..]);
        exc
    }

    /// Voicing decision for concealment, and the energy that restores a
    /// lost first SID frame, from the normalised excitation `xn`.
    fn classify(&mut self, xn: &[i16; HISTORY + FRAME_SAMPLES], lag2: i16) {
        const START: usize = HISTORY + 2 * SUBFRAME;
        const LEN: usize = 2 * SUBFRAME;
        let lag = lag2.min(LAG_SEARCH_MAX) as usize;
        let current = &xn[START..START + LEN];
        let (mut best, mut best_corr) = (lag, 0);
        for t in lag - 3..=lag + 3 {
            let c = dot(current, &xn[START - t..START - t + LEN]);
            if c > best_corr {
                best_corr = c;
                best = t;
            }
        }
        let ten = round16(dot(current, current));
        self.sid_gain = ten;
        let past = &xn[START - best..START - best + LEN];
        let eb = dot(past, past);

        let cc = round16(best_corr);
        self.conceal_lag = if cc <= 0 {
            0
        } else {
            let acc = msu32(shr32(lmul(round16(eb), ten), 3), cc, cc);
            if acc < 0 {
                best as i16
            } else {
                0
            }
        };
    }

    fn concealed_excitation(&mut self) -> [i16; FRAME_SAMPLES] {
        let mut exc = [0i16; FRAME_SAMPLES];
        if self.erase_count >= 3 {
            self.prev_exc = [0; HISTORY];
        } else if self.conceal_lag != 0 {
            let lag = self.conceal_lag as usize;
            let mut x = [0i16; HISTORY + FRAME_SAMPLES];
            x[..HISTORY].copy_from_slice(&self.prev_exc);
            for n in 0..FRAME_SAMPLES {
                x[HISTORY + n] = x[HISTORY - lag + n];
            }
            for n in 0..FRAME_SAMPLES {
                x[HISTORY + n] = mul16(x[HISTORY + n], 0x6000);
                exc[n] = x[HISTORY + n];
            }
            self.prev_exc.copy_from_slice(&x[FRAME_SAMPLES..]);
        } else {
            for sample in &mut exc {
                *sample = mul16(self.conceal_gain, rand16(&mut self.conceal_seed));
            }
            self.prev_exc = [0; HISTORY];
        }
        exc
    }

    /// Excitation and LPC of a comfort noise frame. `sid` holds the LSP and
    /// gain indices of a SID frame; `None` is an untransmitted frame.
    fn comfort_noise(
        &mut self,
        sid: Option<(i32, i16)>,
    ) -> ([i16; FRAME_SAMPLES], [[i16; ORDER]; 4]) {
        let after_speech = self.past_kind == Kind::Active;
        match sid {
            Some((lsp, gain)) => {
                self.sid_gain = sid_gain_decode(gain);
                self.sid_lsp = decode_lsp(lsp, &self.prev_lsp, false);
            }
            None if after_speech => {
                let index = sid_gain_quantize(self.sid_gain, self.cur_gain);
                self.sid_gain = sid_gain_decode(index);
            }
            None => {}
        }
        self.cur_gain = if after_speech {
            self.sid_gain
        } else {
            hi16(add32(
                lmul(self.cur_gain, 0x7000),
                lmul(self.sid_gain, 0x1000),
            ))
        };

        let exc = self.noise_excitation();
        let lpc = subframe_lpc(&self.sid_lsp, &self.prev_lsp);
        self.prev_lsp = self.sid_lsp;
        (exc, lpc)
    }

    /// Draws a random number in 0..n from the comfort noise seed.
    fn draw(&mut self, n: i16) -> i16 {
        mul16(rand16(&mut self.cng_seed) & 0x7fff, n)
    }

    fn noise_excitation(&mut self) -> [i16; FRAME_SAMPLES] {
        const DELTA: [i16; 4] = [1, 0, 1, 3];
        let lags = [self.draw(21) + 123, self.draw(19) + 123];
        let mut gains = [0usize; 4];
        for gain in &mut gains {
            *gain = (self.draw(50) + 1) as usize;
        }

        let mut offset = [0usize; 4];
        let mut signs = [[0i16; 11]; 2];
        for (block, block_signs) in signs.iter_mut().enumerate() {
            let t = self.draw(8192);
            offset[2 * block] = (t & 1) as usize;
            offset[2 * block + 1] = SUBFRAME + ((t >> 1) & 1) as usize;
            for (k, sign) in block_signs.iter_mut().enumerate() {
                *sign = if (t >> (k + 2)) & 1 != 0 {
                    16384
                } else {
                    -16384
                };
            }
        }

        let mut positions = [[0usize; 11]; 2];
        for subframe in 0..4 {
            let mut slots: [usize; 30] = std::array::from_fn(|k| k);
            let mut count = 30;
            let first = if subframe % 2 == 0 {
                0
            } else {
                MPMLQ_PULSE_COUNT[subframe - 1]
            };
            for pulse in 0..MPMLQ_PULSE_COUNT[subframe] {
                let j = self.draw(count as i16) as usize;
                positions[subframe / 2][first + pulse] = 2 * slots[j] + offset[subframe];
                count -= 1;
                slots[j] = slots[count];
            }
        }

        let target = self.cur_gain;
        let mut exc = [0i16; FRAME_SAMPLES];
        for block in 0..2 {
            let mut buf = [0i16; HISTORY + 2 * SUBFRAME];
            buf[..HISTORY].copy_from_slice(&self.prev_exc);
            let mut e = [0i16; 2 * SUBFRAME];
            for half in 0..2 {
                let i = 2 * block + half;
                let u = adaptive_vector(
                    &buf,
                    HISTORY + half * SUBFRAME,
                    lags[block],
                    DELTA[i],
                    gains[i],
                    false,
                );
                e[half * SUBFRAME..(half + 1) * SUBFRAME].copy_from_slice(&u);
            }
            add_noise_pulses(&mut e, &positions[block], &signs[block], target);
            exc[block * 2 * SUBFRAME..(block + 1) * 2 * SUBFRAME].copy_from_slice(&e);
            self.prev_exc.copy_within(2 * SUBFRAME.., 0);
            self.prev_exc[HISTORY - 2 * SUBFRAME..].copy_from_slice(&e);
        }
        exc
    }

    fn synthesize(&mut self, exc: &[i16], a: &[i16; ORDER]) -> [i16; SUBFRAME] {
        let mut y = [0i16; SUBFRAME];
        for (out, &x) in y.iter_mut().zip(exc) {
            let acc = shr32(dep_hi(x), 3);
            let acc = shl32(
                a.iter()
                    .zip(&self.synth_mem)
                    .fold(acc, |acc, (&c, &m)| mac32(acc, c, m)),
                2,
            );
            self.synth_mem.copy_within(..ORDER - 1, 1);
            self.synth_mem[0] = round16(acc);
            *out = if self.postfilter {
                self.synth_mem[0]
            } else {
                shl16(self.synth_mem[0], 1)
            };
        }
        y
    }

    /// Formant postfilter with tilt compensation, in place. Returns the
    /// energy of its input for the gain scaling unit.
    fn formant_postfilter(&mut self, y: &mut [i16; SUBFRAME], a: &[i16; ORDER]) -> i32 {
        let mut zeros = [0i16; ORDER];
        let mut poles = [0i16; ORDER];
        for j in 0..ORDER {
            zeros[j] = mul16r(a[j], POSTFILTER_ZERO_WEIGHTS[j]);
            poles[j] = mul16r(a[j], POSTFILTER_POLE_WEIGHTS[j]);
        }

        let mut t = *y;
        let z = vnorm(&mut t);
        let r1 = dot(&t[1..], &t[..SUBFRAME - 1]);
        let r0 = t[1..]
            .iter()
            .fold(lmul(t[0], t[0]), |acc, &x| mac32(acc, x, x));
        let energy = shr32(r0, 2 * z + 4);

        let h = hi16(r0);
        let k = if h != 0 {
            let r1h = shr32(r1, 1);
            let k = div32x16(abs32(r1h), h);
            if r1h < 0 {
                neg16(k)
            } else {
                k
            }
        } else {
            0
        };
        let acc = mac32(msu32(dep_hi(self.tilt), self.tilt, 0x2000), k, 0x2000);
        self.tilt = round16(acc);
        let mu = mul16(self.tilt, -16384) & !3;

        for sample in y.iter_mut() {
            let input = *sample;
            let mut acc = shr32(dep_hi(input), 2);
            acc = zeros
                .iter()
                .zip(&self.pf_fir_mem)
                .fold(acc, |acc, (&c, &m)| msu32(acc, c, m));
            self.pf_fir_mem.copy_within(..ORDER - 1, 1);
            self.pf_fir_mem[0] = input;
            acc = poles
                .iter()
                .zip(&self.pf_iir_mem)
                .fold(acc, |acc, (&c, &m)| mac32(acc, c, m));
            self.pf_iir_mem.copy_within(..ORDER - 1, 1);
            acc = shl32(acc, 2);
            self.pf_iir_mem[0] = round16(acc);
            *sample = round16(mac32(acc, self.pf_iir_mem[1], mu));
        }
        energy
    }

    /// Scales the postfiltered subframe back to the energy of the synthesis
    /// output, with a smoothed gain.
    fn scale_gain(&mut self, y: &mut [i16; SUBFRAME], energy: i32) {
        let filtered = y
            .iter()
            .fold(0, |acc, &x| mac32(acc, shr16(x, 2), shr16(x, 2)));
        let gain = if filtered != 0 && energy != 0 {
            let e1 = norm32(filtered);
            let filtered = shl32(filtered, e1);
            let e2 = norm32(energy);
            let energy = shr32(shl32(energy, e2), 1);
            let shift = sub16(6, add16(sub16(e1, e2), 1)).max(0);
            let q = div32x16(energy, hi16(filtered));
            isqrt(shr32(dep_hi(q), shift))
        } else {
            0x1000
        };
        for sample in y.iter_mut() {
            let acc = mac32(
                msu32(dep_hi(self.out_gain), self.out_gain, 0x0800),
                gain,
                0x0800,
            );
            self.out_gain = round16(acc);
            let m = add16(self.out_gain, shr16(self.out_gain, 4));
            *sample = round16(shl32(lmul(*sample, m), 4));
        }
    }
}

/// Adds the 11 comfort noise pulses of a 120-sample block to its adaptive
/// excitation `e`, with the gain that brings the block to the target energy.
fn add_noise_pulses(e: &mut [i16; 2 * SUBFRAME], pos: &[usize; 11], sign: &[i16; 11], target: i16) {
    let max = e.iter().fold(0, |m, &x| m.max(abs16(x)));
    let mut k = if max == 0 {
        0
    } else {
        sub16(4, norm16(max)).max(-2)
    };
    let te = e.map(|x| shr16(x, k));
    let eu = dot(&te, &te);
    let cross = pos
        .iter()
        .zip(sign)
        .fold(0, |acc, (&p, &s)| mac32(acc, te[p], s));
    let ie = hi16(shl32(cross, 1));

    let t = lo16(shr32(lmul(target, 60), 6));
    let wanted = shr32(lmul(t, target), add16(shl16(k, 1), 4));
    let c = mul32x16(sub32(eu, wanted), 2979);
    let b0 = mul16r(ie, 2979);
    let d = neg32(msu32(c, b0, b0));
    let x = if d <= 0 {
        neg16(b0)
    } else {
        let root = isqrt(d);
        let x1 = sub16(root, b0);
        let x2 = add16(b0, root);
        if abs16(x2) < abs16(x1) {
            neg16(x2)
        } else {
            x1
        }
    };
    k = add16(k, 1);
    let gain = shl16(x, k).clamp(-10000, 10000);
    for (&p, &s) in pos.iter().zip(sign) {
        e[p] = add16(e[p], mul16(gain, s));
    }
}

/// SID gain value of a 6-bit SID gain index.
fn sid_gain_decode(index: i16) -> i16 {
    let segment = (index >> 4).min(2);
    let i = index - 16 * segment;
    let value = (i << (segment + 1)) + CNG_GAIN_SEGMENT_BASE[segment as usize];
    value * 32
}

/// SID gain index nearest an excitation energy `energy` stored with its
/// normalisation `shift`.
fn sid_gain_quantize(energy: i16, shift: i16) -> i16 {
    let s = sub16(16, shl16(shift, 1));
    let x = mul32x16(shl32(i32::from(energy), s), 273);
    if x >= CNG_GAIN_SEGMENT_BOUNDS[2] {
        return 63;
    }
    let (segment, steps) = if x >= CNG_GAIN_SEGMENT_BOUNDS[1] {
        (2, 4)
    } else if x >= CNG_GAIN_SEGMENT_BOUNDS[0] {
        (1, 3)
    } else {
        (0, 3)
    };
    let base = CNG_GAIN_SEGMENT_BASE[segment];
    let level = |j: i16| base + (j << (segment + 1));

    let mut j: i16 = 1 << steps;
    let mut step = j / 2;
    for _ in 0..steps {
        let v = level(j);
        if x >= lmul(v, v) {
            j += step;
        } else {
            j -= step;
        }
        step /= 2;
    }

    let v = level(j);
    let d = sub32(lmul(v, v), x);
    let first = 16 * segment as i16;
    if d <= 0 {
        let v2 = level(j + 1);
        let d2 = sub32(x, lmul(v2, v2));
        if d > d2 {
            first + j
        } else {
            first + j + 1
        }
    } else {
        let v2 = level(j - 1);
        let d2 = sub32(x, lmul(v2, v2));
        if d < d2 {
            first + j
        } else {
            first + j - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_bit_primitives_floor_and_saturate() {
        assert_eq!(add16(32000, 1000), i16::MAX);
        assert_eq!(sub16(-32000, 1000), i16::MIN);
        assert_eq!(neg16(i16::MIN), i16::MAX);
        assert_eq!(abs16(i16::MIN), i16::MAX);
        assert_eq!(mul16(-1, 1), -1);
        assert_eq!(mul16(3, -16384), -2);
        assert_eq!(mul16(i16::MIN, i16::MIN), i16::MAX);
        assert_eq!(mul16r(3, -16384), -1);
        assert_eq!(mul16r(i16::MIN, i16::MIN), i16::MAX);
        assert_eq!(shl16(1, 15), i16::MAX);
        assert_eq!(shl16(-1, 16), i16::MIN);
        assert_eq!(shl16(0, 20), 0);
        assert_eq!(shl16(-12, -2), -3);
        assert_eq!(shr16(-3, 1), -2);
        assert_eq!(shr16(-1, 20), -1);
        assert_eq!(shr16(5, -2), 20);
        assert_eq!(div16(1, 3), 10922);
        assert_eq!(div16(7, 7), i16::MAX);
        for (a, n) in [
            (0i16, 0i16),
            (-1, 15),
            (1, 14),
            (16384, 0),
            (-32768, 0),
            (-16385, 0),
            (-16384, 1),
        ] {
            assert_eq!(norm16(a), n, "norm16({a})");
        }
    }

    #[test]
    fn thirty_two_bit_primitives_floor_and_saturate() {
        assert_eq!(lmul(i16::MIN, i16::MIN), i32::MAX);
        assert_eq!(lmul(-3, 5), -30);
        assert_eq!(mac32(i32::MAX - 1, 1, 1), i32::MAX);
        assert_eq!(msu32(i32::MIN + 1, 1, 1), i32::MIN);
        assert_eq!(neg32(i32::MIN), i32::MAX);
        assert_eq!(abs32(i32::MIN), i32::MAX);
        assert_eq!(shl32(1, 31), i32::MAX);
        assert_eq!(shl32(-1, 40), i32::MIN);
        assert_eq!(shr32(-5, 1), -3);
        assert_eq!(shr32(-5, 40), -1);
        assert_eq!(shr32(3, -2), 12);
        assert_eq!(hi16(-1), -1);
        assert_eq!(lo16(0x0001_8000), i16::MIN);
        assert_eq!(round16(0x7fff), 0);
        assert_eq!(round16(0x8000), 1);
        assert_eq!(round16(i32::MAX), i16::MAX);
        assert_eq!(round16(i32::MIN), i16::MIN);
        for (x, n) in [(0, 0), (-1, 31), (1, 30), (1 << 30, 0), (i32::MIN, 0)] {
            assert_eq!(norm32(x), n, "norm32({x})");
        }
        assert_eq!(mul32x16(0x0001_8000, 16384), 0xc000);
        assert_eq!(mul32x16(-1, 16384), -1);
        assert_eq!(mul32x16(i32::MAX, i16::MIN), -i32::MAX);
        assert_eq!(div32x16(100, 1), 50);
        assert_eq!(div32x16(65536, 1), i16::MAX);
    }

    #[test]
    fn composite_routines() {
        assert_eq!(isqrt(7), 0);
        assert_eq!(isqrt(8), 2);
        assert_eq!(isqrt(20000), 100);
        assert_eq!(isqrt(20001), 100);
        assert_eq!(isqrt(-5), 0);
        assert_eq!(isqrt(i32::MAX), 32766);

        let mut seed = 12345;
        assert_eq!(rand16(&mut seed), 9476);
        assert_eq!(rand16(&mut seed), 22055);
        assert_eq!(seed, 22055);
        let mut seed = 0;
        assert_eq!(rand16(&mut seed), 259);

        let mut v = [1i16, -2, 0];
        assert_eq!(vnorm(&mut v), 10);
        assert_eq!(v, [1024, -2048, 0]);
        let mut zeros = [0i16; 4];
        assert_eq!(vnorm(&mut zeros), -3);
        assert_eq!(zeros, [0; 4]);
    }

    #[test]
    fn frame_size_follows_the_type_bits() {
        assert_eq!(frame_size(0x00), 24);
        assert_eq!(frame_size(0xfd), 20);
        assert_eq!(frame_size(0x02), 4);
        assert_eq!(frame_size(0x07), 1);
    }

    /// Writes fields from their least significant bit up, like a frame.
    struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }

        fn put(&mut self, value: u32, width: usize) -> &mut Self {
            self.bits
                .extend((0..width).map(|t| ((value >> t) & 1) as u8));
            self
        }

        fn bytes(&self, len: usize) -> Vec<u8> {
            let mut out = vec![0u8; len];
            for (i, &bit) in self.bits.iter().enumerate() {
                out[i / 8] |= bit << (i % 8);
            }
            out
        }
    }

    fn high_rate_frame(lag0_code: u32, gain0: u32) -> Vec<u8> {
        high_rate_frame_with_lsp(0x00ab_cdef, lag0_code, gain0)
    }

    fn high_rate_frame_with_lsp(lsp: u32, lag0_code: u32, gain0: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(0, 2).put(lsp, 24);
        w.put(lag0_code, 7).put(2, 2).put(100, 7).put(3, 2);
        for gain in [gain0, 0x123, 0x456, 0x789] {
            w.put(gain, 12);
        }
        w.put(0b0101, 4).put(1, 1);
        w.put(21 * 90 + 41, 13);
        w.put(0x1234, 16)
            .put(0x0567, 14)
            .put(0x89ab, 16)
            .put(0x0cde, 14);
        w.put(0b101010, 6)
            .put(0b10101, 5)
            .put(0b111111, 6)
            .put(0, 5);
        w.bytes(24)
    }

    #[test]
    fn parse_reads_a_high_rate_frame() {
        // Lag code 30 is under the 58 threshold for subframes 0 and 1: their
        // gain code carries the pulse train bit at bit 11.
        let frame = match parse(&high_rate_frame(30, 0x800 | (20 * 24 + 7))) {
            Frame::Active(frame) => frame,
            other => panic!("{other:?}"),
        };
        assert_eq!(frame.rate, Rate::High);
        assert_eq!(frame.lsp, 0x00ab_cdef);
        assert_eq!(frame.lag, [48, 118]);
        assert_eq!(frame.delta, [1, 2, 1, 3]);
        assert_eq!(frame.train, [true, false, false, false]);
        assert_eq!(frame.acb_gain, [20, 0x123 / 24, 0x456 / 24, 0x789 / 24]);
        assert_eq!(frame.fcb_gain, [7, 0x123 % 24, 0x456 % 24, 0x789 % 24]);
        assert_eq!(frame.grid, [1, 0, 1, 0]);
        assert_eq!(
            frame.pos,
            [
                2 * 65536 + 0x1234,
                3 * 16384 + 0x0567,
                4 * 65536 + 0x89ab,
                5 * 16384 + 0x0cde
            ]
        );
        assert_eq!(frame.sign, [0b101010, 0b10101, 0b111111, 0]);
    }

    #[test]
    fn parse_turns_forbidden_codes_into_bad_frames() {
        assert!(matches!(parse(&high_rate_frame(123, 0)), Frame::Active(_)));
        assert_eq!(parse(&high_rate_frame(124, 0)), Frame::Bad);
        // 85-entry book under the threshold, 170-entry book above it.
        assert_eq!(parse(&high_rate_frame(30, 85 * 24)), Frame::Bad);
        assert!(matches!(
            parse(&high_rate_frame(40, 169 * 24)),
            Frame::Active(_)
        ));
        assert_eq!(parse(&high_rate_frame(40, 170 * 24)), Frame::Bad);
    }

    #[test]
    fn parse_reads_low_rate_sid_and_untransmitted_frames() {
        let mut w = BitWriter::new();
        w.put(1, 2)
            .put(0x0012_3456, 24)
            .put(10, 7)
            .put(0, 2)
            .put(80, 7)
            .put(1, 2);
        for gain in [169 * 24, 5, 6, 7] {
            w.put(gain, 12);
        }
        w.put(0b0011, 4);
        for pos in [0xfff, 1, 2, 3] {
            w.put(pos, 12);
        }
        w.put(1, 4).put(2, 4).put(4, 4).put(8, 4);
        let frame = match parse(&w.bytes(20)) {
            Frame::Active(frame) => frame,
            other => panic!("{other:?}"),
        };
        assert_eq!(frame.rate, Rate::Low);
        assert_eq!(frame.lag, [28, 98]);
        assert_eq!(frame.train, [false; 4]);
        assert_eq!(frame.acb_gain, [169, 0, 0, 0]);
        assert_eq!(frame.grid, [1, 1, 0, 0]);
        assert_eq!(frame.pos, [0xfff, 1, 2, 3]);
        assert_eq!(frame.sign, [1, 2, 4, 8]);

        let mut w = BitWriter::new();
        w.put(2, 2).put(0x00fe_dcba, 24).put(45, 6);
        assert_eq!(
            parse(&w.bytes(4)),
            Frame::Sid {
                lsp: 0x00fe_dcba,
                gain: 45
            }
        );
        assert_eq!(parse(&[0xff]), Frame::Untransmitted);
    }

    #[test]
    fn sid_gain_levels() {
        for (index, gain) in [
            (0, 0),
            (1, 64),
            (15, 960),
            (16, 1024),
            (31, 2944),
            (32, 3072),
            (63, 11008),
        ] {
            assert_eq!(sid_gain_decode(index), gain, "{index}");
        }
        // Silence quantizes to the lowest level, a loud frame to the top one.
        assert_eq!(sid_gain_quantize(0, 0), 0);
        assert_eq!(sid_gain_quantize(i16::MAX, -3), 63);
    }

    /// Three 6.3 kbit/s frames of tests/fixtures/dssds2/ds2/olympus-lp.ds2,
    /// a 5.3 kbit/s frame, a SID frame, two untransmitted frames and the
    /// first frame again.
    const STREAM: &str = "448699040a0400000080005006f9d78384e3180900a8d552a0722008222e0091a2f301be\
        443ed48370b19b77d9de1041981ead10201417d227147211e8d843be3b41e4ff108dde92c5f168a1925912\
        d73296ef6f23614589c7aba5c30e0804500303448699040a0400000080005006f9d78384e3180900a8d552";

    fn frames() -> Vec<Vec<u8>> {
        let bytes: Vec<u8> = (0..STREAM.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&STREAM[i..i + 2], 16).unwrap())
            .collect();
        let mut frames = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let end = pos + frame_size(bytes[pos]);
            frames.push(bytes[pos..end].to_vec());
            pos = end;
        }
        frames
    }

    #[test]
    fn decodes_a_short_stream() {
        let frames = frames();
        assert_eq!(
            frames.iter().map(Vec::len).collect::<Vec<_>>(),
            [24, 24, 24, 20, 4, 1, 1, 24]
        );
        let mut decoder = G7231Decoder::new();
        let samples: Vec<i16> = frames
            .iter()
            .flat_map(|f| decoder.decode_frame(f))
            .collect();
        assert_eq!(samples.len(), 8 * FRAME_SAMPLES);
        // FFmpeg's G.723.1 decoder gives the same samples.
        let checksum: i64 = samples
            .iter()
            .enumerate()
            .map(|(i, &s)| (i as i64 + 1) * i64::from(s))
            .sum();
        assert_eq!(checksum, 768637);
        assert_eq!(samples[720..724], [96, 131, 129, 126]);
        let peak = |range: std::ops::Range<usize>| {
            samples[range]
                .iter()
                .map(|s| s.unsigned_abs())
                .max()
                .unwrap()
        };
        assert_eq!(peak(960..1200), 22307);
        assert_eq!(peak(1200..1920), 22067);
    }

    #[test]
    fn decoder_state_carries_from_frame_to_frame() {
        let frames = frames();
        let mut first = G7231Decoder::new();
        let mut second = G7231Decoder::new();
        assert_eq!(
            first.decode_frame(&frames[0]),
            second.decode_frame(&frames[0])
        );
        assert_eq!(
            first.decode_frame(&frames[1]),
            second.decode_frame(&frames[1])
        );
        // A lost frame is concealed from the speech before it.
        let concealed = first.decode_erased();
        assert_ne!(concealed, [0; FRAME_SAMPLES]);
        assert_ne!(concealed, second.decode_frame(&frames[2]));
        // An empty frame is an erased one.
        let mut empty = G7231Decoder::new();
        let mut erased = G7231Decoder::new();
        assert_eq!(
            empty.decode_frame(&frames[0]),
            erased.decode_frame(&frames[0])
        );
        assert_eq!(empty.decode_frame(&[]), erased.decode_erased());
        // With no speech before it, concealment has nothing to repeat.
        assert_eq!(G7231Decoder::new().decode_erased(), [0; FRAME_SAMPLES]);
    }

    /// The cosine of an LSP, computed the other way: the unsigned 16-bit
    /// pattern of q gives the table index mod 512 and the fraction.
    fn lsp_cosine_unsigned(q: i16) -> i16 {
        let u = q as u16;
        let j = usize::from(u >> 7);
        let low = COSINE[j];
        let high = COSINE[(j + 1) % 512];
        let fraction = add16(shl16((u & 0x7f) as i16, 8), 0x80);
        let acc = shl32(mac32(dep_hi(low), sub16(high, low), fraction), 1);
        neg16(round16(acc))
    }

    #[test]
    fn lsp_cosine_reads_the_table_periodically() {
        for q in i16::MIN..=i16::MAX {
            assert_eq!(lsp_cosine(q), lsp_cosine_unsigned(q), "{q}");
        }
        // -2 cos(0) and -2 cos(pi), about, in the scale of the conversion.
        assert_eq!(lsp_cosine(0), -32767);
        assert_eq!(lsp_cosine(-1), lsp_cosine(0));
        // The top of the int16 range reads entries 255 and 256.
        assert_eq!(lsp_cosine(i16::MAX), lsp_cosine_unsigned(i16::MAX));
    }

    #[test]
    fn decodes_frames_that_drive_an_lsp_below_zero() {
        // Band 0 entry 0x43 with the band 1 and 2 entries 0: the first LSP
        // goes below zero on the fourth frame and settles at -26, and the
        // conversion reads the cosine table at index -1.
        let frame = high_rate_frame_with_lsp(0x0043_0000, 40, 100);
        let mut decoder = G7231Decoder::new();
        let mut samples = Vec::new();
        let mut lowest = i16::MAX;
        for _ in 0..8 {
            samples.extend(decoder.decode_frame(&frame));
            lowest = lowest.min(decoder.prev_lsp[0]);
        }
        assert_eq!(lowest, -26);
        let checksum: i64 = samples
            .iter()
            .enumerate()
            .map(|(i, &s)| (i as i64 + 1) * i64::from(s))
            .sum();
        // FFmpeg's G.723.1 decoder gives the same samples.
        assert_eq!(checksum, 7318052758);
    }

    /// Frames of random bytes of every type, cut short, empty or erased,
    /// with the postfilter on and off: the decoder never panics.
    #[test]
    fn random_bytes_never_panic() {
        let mut state: u32 = 0x1234_5678;
        let mut next = move || {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (state >> 16) as u8
        };
        let mut on = G7231Decoder::new();
        let mut off = G7231Decoder::with_postfilter(false);
        for _ in 0..4000 {
            let kind = next() & 3;
            let mut frame: Vec<u8> = (0..frame_size(kind)).map(|_| next()).collect();
            frame[0] = frame[0] & !3 | kind;
            match next() % 16 {
                0 => frame.clear(),
                1 => frame.truncate(usize::from(next()) % frame.len()),
                _ => {}
            }
            if next() % 20 == 0 {
                on.decode_erased();
                off.decode_erased();
            } else {
                on.decode_frame(&frame);
                off.decode_frame(&frame);
            }
        }
    }

    #[test]
    fn postfilter_switch_changes_the_output() {
        let frames = frames();
        let mut on = G7231Decoder::new();
        let mut off = G7231Decoder::with_postfilter(false);
        let a: Vec<i16> = frames.iter().flat_map(|f| on.decode_frame(f)).collect();
        let b: Vec<i16> = frames.iter().flat_map(|f| off.decode_frame(f)).collect();
        assert_eq!(a.len(), b.len());
        assert_ne!(a, b);
    }
}
