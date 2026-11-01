//! Grundig DSS demuxer.
//!
//! Grundig `\x06dss` files store the CELP bitstream in 512-byte blocks. Each
//! block begins with a 6-byte header `[b0][b1][b2] ff 00 ff`; the remaining 506
//! bytes are payload, stored as little-endian 16-bit words whose bits are read
//! MSB-first. `block_frames()` gives the number of frames that *start* within
//! the block, and `block_start_bits()` how many bits from the start of the
//! block the first of them starts. In this header that is `b1*16 + (b0>>4)`
//! without its low 3 bits, since the first frame starts on a whole byte.
//! The demuxer takes off the 48 header bits to get the offset into the payload.
//! An offset that lands inside the header reaches back into the previous
//! block when bit 3 of the pointer is set, and counts as 0 otherwise. Frames
//! are a continuous 328-bit (41-byte) stream; a frame may spill into the next block,
//! but each block's frames always start at that block's own first-frame offset.
//!
//! The whole audio region is buffered and demuxed at `finish`, because frame
//! extraction needs each block's `frame_count` and the cross-block bit reads.

use crate::demux::dss::{
    block_frames, block_start_bits, header_format, part_format, DssSpStreamDemuxer, TaggedFrames,
};
use crate::error::{DecodeError, Result};

const BLOCK_SIZE: usize = 512;
const HEADER_SIZE: usize = 6;
const FRAME_BITS: usize = 328;

/// Read a single bit (MSB-first) from a payload stored as little-endian
/// 16-bit words. Out-of-range reads return 0, matching the reference.
#[inline]
fn read_bit(payload: &[u8], bit_index: usize) -> u8 {
    let wi = bit_index / 16;
    let bit = bit_index % 16;
    if wi * 2 + 1 >= payload.len() {
        0
    } else {
        let w = ((payload[wi * 2 + 1] as u16) << 8) | (payload[wi * 2] as u16);
        ((w >> (15 - bit)) & 1) as u8
    }
}

/// Demux a Grundig audio region (after the header blocks) into 41-byte frames.
pub fn demux_grundig(audio: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    if let Some(other) = other_part(audio) {
        return Err(DecodeError::MultipleParts(audio[other * BLOCK_SIZE + 4]));
    }
    let (frames, _cut) = grundig_frames(audio);
    let total = frames.len();
    Ok((frames, total))
}

fn other_part(audio: &[u8]) -> Option<usize> {
    audio.chunks(BLOCK_SIZE).position(|blk| {
        blk.len() >= HEADER_SIZE
            && block_frames(blk) > 0
            && part_format(header_format(blk)).is_some_and(|format| format != 0)
    })
}

fn grundig_frames(audio: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut payload = Vec::with_capacity(audio.len());
    let mut blocks = Vec::new();
    for blk in audio.chunks(BLOCK_SIZE) {
        if blk.len() < HEADER_SIZE {
            break;
        }
        let mut first = block_start_bits(blk) as isize - (HEADER_SIZE * 8) as isize;
        let mut count = block_frames(blk);
        let back = block_start_bits(blk) & 8 != 0;
        if first < 0 && back && payload.is_empty() {
            while first < 0 && count > 0 {
                first += FRAME_BITS as isize;
                count -= 1;
            }
        }
        if first < 0 && (!back || payload.is_empty()) {
            first = 0;
        }
        blocks.push((((payload.len() * 8) as isize + first) as usize, count));
        payload.extend_from_slice(&blk[HEADER_SIZE..]);
    }
    let payload_bits = payload.len() / 2 * 16;

    let mut frames = Vec::new();
    for (first, count) in blocks {
        for pos in (first..).step_by(FRAME_BITS).take(count) {
            if pos + FRAME_BITS > payload_bits {
                return (frames, true);
            }
            let bits: Vec<u8> = (pos..pos + FRAME_BITS)
                .map(|i| read_bit(&payload, i))
                .collect();
            frames.push(pack_frame(&bits));
        }
    }
    (frames, false)
}

/// Pack 328 MSB-first bits into a 41-byte frame.
fn pack_frame(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 41];
    for (i, &bit) in bits.iter().enumerate().take(FRAME_BITS) {
        if bit != 0 {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

/// Streaming wrapper: buffers the whole file and demuxes at `finish`.
pub(crate) struct GrundigSpStreamDemuxer {
    header_size: usize,
    buf: Vec<u8>,
}

impl GrundigSpStreamDemuxer {
    /// `header_size` = offset of the first audio block.
    pub(crate) fn new(header_size: usize) -> Self {
        Self {
            header_size,
            buf: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub(crate) fn finish(&mut self) -> Result<TaggedFrames> {
        self.check_header()?;
        self.demux_all(true)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<TaggedFrames> {
        self.check_header()?;
        self.demux_all(false)
    }

    fn check_header(&self) -> Result<()> {
        if !self.buf.is_empty() && self.buf.len() < self.header_size {
            return Err(DecodeError::Truncated("Grundig header".to_string()));
        }
        Ok(())
    }

    fn demux_all(&mut self, strict: bool) -> Result<TaggedFrames> {
        if self.buf.len() <= self.header_size {
            return Ok(Vec::new());
        }
        let audio = &self.buf[self.header_size..];
        let Some(other) = other_part(audio) else {
            let (frames, cut) = grundig_frames(audio);
            if strict && cut && !audio.len().is_multiple_of(BLOCK_SIZE) {
                return Err(DecodeError::Truncated("Grundig SP frame".to_string()));
            }
            return Ok(frames.into_iter().map(|frame| (0, frame)).collect());
        };

        let mut frames: TaggedFrames = grundig_frames(&audio[..other * BLOCK_SIZE])
            .0
            .into_iter()
            .map(|frame| (0, frame))
            .collect();
        let mut rest = DssSpStreamDemuxer::from_block(
            self.header_size,
            self.header_size + other * BLOCK_SIZE,
            0,
        );
        let mut more = rest.push(&self.buf)?;
        more.extend(if strict {
            rest.finish()?
        } else {
            rest.finish_lenient()?
        });
        let lead = more.iter().take_while(|(tag, _)| *tag == 0).count();
        for (index, (tag, packet)) in more.into_iter().enumerate() {
            if index < lead {
                frames.push((0, packet_frame(&packet)));
            } else {
                frames.push((tag, packet));
            }
        }
        Ok(frames)
    }
}

fn packet_frame(packet: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8; FRAME_BITS / 8];
    for (i, byte) in frame.iter_mut().enumerate() {
        *byte = packet.get(i ^ 1).copied().unwrap_or(0);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(b0: u8, b1: u8, count: u8, seed: u8) -> Vec<u8> {
        let mut blk = vec![0u8; BLOCK_SIZE];
        blk[..HEADER_SIZE].copy_from_slice(&[b0, b1, count, 0xff, 0x00, 0xff]);
        for (i, byte) in blk[HEADER_SIZE..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(seed);
        }
        blk
    }

    fn payload_bits(audio: &[u8]) -> Vec<u8> {
        let mut bits = Vec::new();
        for blk in audio.chunks(BLOCK_SIZE) {
            for word in blk[HEADER_SIZE..].chunks_exact(2) {
                let word = u16::from_le_bytes([word[0], word[1]]);
                bits.extend((0..16).rev().map(|k| ((word >> k) & 1) as u8));
            }
        }
        bits
    }

    fn frame_at(bits: &[u8], start: usize) -> Vec<u8> {
        let mut frame = vec![0u8; 41];
        for (i, &bit) in bits[start..start + FRAME_BITS].iter().enumerate() {
            frame[i / 8] |= bit << (7 - i % 8);
        }
        frame
    }

    fn frames_from(bits: &[u8], count: usize) -> Vec<Vec<u8>> {
        (0..count).map(|k| frame_at(bits, k * FRAME_BITS)).collect()
    }

    fn grundig_file(audio: &[u8]) -> Vec<u8> {
        let mut file = vec![0u8; 6 * BLOCK_SIZE];
        file[..4].copy_from_slice(b"\x06dss");
        file.extend_from_slice(audio);
        file
    }

    fn stream_finish(file: &[u8], strict: bool) -> Result<Vec<Vec<u8>>> {
        let mut demuxer = GrundigSpStreamDemuxer::new(6 * BLOCK_SIZE);
        for piece in file.chunks(37) {
            assert!(demuxer.push(piece).unwrap().is_empty());
        }
        let frames = if strict {
            demuxer.finish()
        } else {
            demuxer.finish_lenient()
        };
        frames.map(untagged)
    }

    fn untagged(frames: TaggedFrames) -> Vec<Vec<u8>> {
        frames.into_iter().map(|(_, frame)| frame).collect()
    }

    #[test]
    fn test_grundig_frames_continue_into_next_block() {
        let data = [block(0x00, 255, 2, 1), block(0x00, 53, 1, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            frames,
            vec![
                frame_at(&bits, 4032),
                frame_at(&bits, 4360),
                frame_at(&bits, 4048 + 800)
            ]
        );
    }

    #[test]
    fn test_grundig_block_frames_start_at_own_offset() {
        let data = [block(0x00, 3, 1, 1), block(0x40, 13, 2, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            frames,
            vec![
                frame_at(&bits, 0),
                frame_at(&bits, 4048 + 160),
                frame_at(&bits, 4048 + 488)
            ]
        );
    }

    #[test]
    fn test_grundig_start_drops_the_low_3_bits_of_the_pointer() {
        for top in 0u8..16 {
            let data = [block(0x00, 3, 1, 1), block(top << 4, 13, 2, 2)].concat();
            let bits = payload_bits(&data);
            let start = 4048 + if top < 8 { 160 } else { 168 };
            let expected = vec![
                frame_at(&bits, 0),
                frame_at(&bits, start),
                frame_at(&bits, start + FRAME_BITS),
            ];
            assert_eq!(demux_grundig(&data).unwrap(), (expected, 3), "{top:x}");
        }
    }

    #[test]
    fn test_grundig_partial_last_block_gives_its_frames() {
        let whole = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&whole);

        let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + HEADER_SIZE + 70]).unwrap();
        assert_eq!(total, 14);
        let mut expected = frames_from(&bits, 13);
        expected.push(frame_at(&bits, 4048 + 216));
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_grundig_stream_demux_matches_batch() {
        let audio = [
            block(0x0f, 3, 12, 1),
            block(0x80, 16, 12, 2),
            block(0x80, 9, 5, 3),
        ]
        .concat();
        let audio = &audio[..2 * BLOCK_SIZE + 300];
        let mut file = vec![0u8; 6 * BLOCK_SIZE];
        file[..4].copy_from_slice(b"\x06dss");
        file.extend_from_slice(audio);

        let (expected, total) = demux_grundig(audio).unwrap();
        assert_eq!(total, 29);
        let mut strict = GrundigSpStreamDemuxer::new(6 * BLOCK_SIZE);
        let mut lenient = GrundigSpStreamDemuxer::new(6 * BLOCK_SIZE);
        for piece in file.chunks(37) {
            assert!(strict.push(piece).unwrap().is_empty());
            assert!(lenient.push(piece).unwrap().is_empty());
        }
        assert_eq!(untagged(strict.finish().unwrap()), expected);
        assert_eq!(untagged(lenient.finish_lenient().unwrap()), expected);
    }

    #[test]
    fn test_grundig_strict_finish_reports_cut_frame() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let short = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        for audio in [
            &long[..BLOCK_SIZE + HEADER_SIZE + 20],
            &short[..BLOCK_SIZE + HEADER_SIZE],
        ] {
            let file = grundig_file(audio);
            assert!(matches!(
                stream_finish(&file, true),
                Err(DecodeError::Truncated(what)) if what == "Grundig SP frame"
            ));
            let lenient = stream_finish(&file, false).unwrap();
            assert!(!lenient.is_empty());
            assert_eq!(lenient, demux_grundig(audio).unwrap().0);
        }
    }

    #[test]
    fn test_grundig_strict_finish_reports_frame_into_cut_block_header() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&long);
        for cut in [1, 3, HEADER_SIZE] {
            let audio = &long[..BLOCK_SIZE + cut];
            let file = grundig_file(audio);
            assert!(matches!(
                stream_finish(&file, true),
                Err(DecodeError::Truncated(what)) if what == "Grundig SP frame"
            ));
            assert_eq!(stream_finish(&file, false).unwrap(), frames_from(&bits, 12));
            assert_eq!(demux_grundig(audio).unwrap().0, frames_from(&bits, 12));
        }
    }

    #[test]
    fn test_grundig_strict_finish_accepts_block_boundary() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let overstated = block(0x00, 3, 13, 1);
        for audio in [&long[..BLOCK_SIZE], &overstated[..]] {
            let file = grundig_file(audio);
            let strict = stream_finish(&file, true).unwrap();
            assert_eq!(strict.len(), 12);
            assert_eq!(strict, stream_finish(&file, false).unwrap());
            assert_eq!(strict, demux_grundig(audio).unwrap().0);
        }
    }

    #[test]
    fn test_grundig_finish_reports_cut_header() {
        let file = grundig_file(&block(0x00, 3, 2, 1));
        for strict in [true, false] {
            for cut in [1, 1000, 6 * BLOCK_SIZE - 1] {
                assert!(matches!(
                    stream_finish(&file[..cut], strict),
                    Err(DecodeError::Truncated(what)) if what == "Grundig header"
                ));
            }
            assert!(stream_finish(&file[..6 * BLOCK_SIZE], strict).unwrap().is_empty());
        }
    }

    #[test]
    fn test_grundig_strict_finish_accepts_cut_block_header() {
        let whole = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        let bits = payload_bits(&whole);

        for cut in [BLOCK_SIZE + 1, BLOCK_SIZE + HEADER_SIZE - 1] {
            let file = grundig_file(&whole[..cut]);
            assert_eq!(stream_finish(&file, true).unwrap(), frames_from(&bits, 2));
            assert_eq!(stream_finish(&file, false).unwrap(), frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_offset_into_header_starts_at_payload() {
        for (b0, b1) in [(0x70, 2), (0x10, 0), (0x70, 1)] {
            let data = block(b0, b1, 2, 1);
            let bits = payload_bits(&data);

            let (frames, total) = demux_grundig(&data).unwrap();
            assert_eq!(total, 2);
            assert_eq!(frames, frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_swapped_start_before_the_payload() {
        for (b0, b1, start) in [(0x80, 2, 8), (0xf0, 2, 8), (0xf0, 1, 24), (0x80, 0, 40)] {
            let data = [block(0x00, 3, 0, 1), block(b0, b1, 2, 2)].concat();
            let bits = payload_bits(&data);
            let pos = 4048 - start;
            let expected = vec![frame_at(&bits, pos), frame_at(&bits, pos + FRAME_BITS)];
            assert_eq!(
                demux_grundig(&data).unwrap(),
                (expected.clone(), 2),
                "{b0:x} {b1}"
            );
            let file = grundig_file(&data);
            assert_eq!(stream_finish(&file, true).unwrap(), expected);

            let alone = block(b0, b1, 2, 2);
            let bits = payload_bits(&alone);
            let expected = vec![frame_at(&bits, FRAME_BITS - start)];
            assert_eq!(
                demux_grundig(&alone).unwrap(),
                (expected, 1),
                "{b0:x} {b1} alone"
            );
        }
    }

    #[test]
    fn test_swapped_frame_after_a_grundig_part_reads_the_block_in_front() {
        let mut first = block(0x80, 3, 13, 5);
        first[4] = 1;
        let mut second = block(0x00, 17, 5, 6);
        second[4] = 1;
        let mut stream = first[HEADER_SIZE..].to_vec();
        stream.extend_from_slice(&second[HEADER_SIZE..]);
        let payload = BLOCK_SIZE - HEADER_SIZE;
        for (count, kept) in [(2u8, 2), (13, 12)] {
            let grundig = block(0x00, 3, count, 1);
            let in_place = [grundig.clone(), first.clone(), second.clone()].concat();
            let expected = stream_finish(&grundig_file(&in_place), false).unwrap();
            assert_eq!(expected.len(), kept + 18);
            for moved in [2usize, 4, 6] {
                let (mut g, mut a, mut b) = (grundig.clone(), first.clone(), second.clone());
                a[1] -= moved as u8 / 2;
                b[1] -= moved as u8 / 2;
                a[HEADER_SIZE..].copy_from_slice(&stream[moved..moved + payload]);
                b[HEADER_SIZE..].fill(0);
                b[HEADER_SIZE..BLOCK_SIZE - moved].copy_from_slice(&stream[moved + payload..]);
                g[BLOCK_SIZE - moved..].copy_from_slice(&stream[..moved]);
                let file = grundig_file(&[g, a, b].concat());
                assert_eq!(
                    stream_finish(&file, false).unwrap(),
                    expected,
                    "{count} frames, {moved} bytes"
                );
            }
        }
    }

    #[test]
    fn test_grundig_drops_frame_cut_by_end_of_file() {
        let whole = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&whole);

        let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + HEADER_SIZE + 20]).unwrap();
        assert_eq!(total, 12);
        assert_eq!(frames, frames_from(&bits, 12));
    }

    #[test]
    fn test_grundig_overstated_count_stops_at_end_of_file() {
        let data = block(0x00, 3, 13, 1);
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 12);
        assert_eq!(frames, frames_from(&bits, 12));
    }

    #[test]
    fn test_grundig_count_ff_holds_no_frames() {
        let data = [block(0x00, 3, 0xff, 1), block(0x00, 3, 2, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames, vec![frame_at(&bits, 4048), frame_at(&bits, 4048 + 328)]);
    }

    #[test]
    fn test_grundig_start_from_byte3_when_pointer_is_0() {
        let data = block(0x00, 0, 2, 1);
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames, vec![frame_at(&bits, 200), frame_at(&bits, 200 + 328)]);
    }

    #[test]
    fn test_grundig_start_from_byte3_before_the_payload() {
        for (byte3, start) in [(0x28u8, 8), (0x2f, 8), (0x18, 24), (0x08, 40)] {
            let mut pause = block(0x00, 0, 2, 2);
            pause[3] = byte3;
            let data = [block(0x00, 3, 0, 1), pause.clone()].concat();
            let bits = payload_bits(&data);
            let pos = 4048 - start;
            let expected = vec![frame_at(&bits, pos), frame_at(&bits, pos + FRAME_BITS)];
            assert_eq!(demux_grundig(&data).unwrap(), (expected, 2), "{byte3:x}");
        }
        for byte3 in [0x20u8, 0x27, 0x10, 0x00] {
            let mut pause = block(0x00, 0, 2, 2);
            pause[3] = byte3;
            let data = [block(0x00, 3, 0, 1), pause].concat();
            let bits = payload_bits(&data);
            let expected = vec![frame_at(&bits, 4048), frame_at(&bits, 4048 + FRAME_BITS)];
            assert_eq!(demux_grundig(&data).unwrap(), (expected, 2), "{byte3:x}");
        }
    }

    #[test]
    fn test_grundig_overstated_count_reads_into_next_block() {
        let data = [block(0x00, 10, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data).unwrap();
        assert_eq!(total, 14);
        let mut expected: Vec<Vec<u8>> = (0..13).map(|k| frame_at(&bits, 112 + k * FRAME_BITS)).collect();
        expected.push(frame_at(&bits, 4048 + 216));
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_grundig_file_cut_inside_block_header() {
        let whole = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        let bits = payload_bits(&whole);

        for cut in [1, HEADER_SIZE - 1, HEADER_SIZE, HEADER_SIZE + 1] {
            let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + cut]).unwrap();
            assert_eq!(total, 2);
            assert_eq!(frames, frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_frame_needs_whole_last_word() {
        let data = block(0x00, 3, 1, 1);
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data[..HEADER_SIZE + 41]).unwrap();
        assert_eq!(total, 0);
        assert!(frames.is_empty());

        let (frames, total) = demux_grundig(&data[..HEADER_SIZE + 42]).unwrap();
        assert_eq!(total, 1);
        assert_eq!(frames, frames_from(&bits, 1));
    }

    #[test]
    fn test_grundig_blocks_without_frames_give_nothing() {
        let data = [block(0x00, 3, 0, 1), block(0x00, 3, 0, 2)].concat();
        assert_eq!(demux_grundig(&data).unwrap(), (Vec::<Vec<u8>>::new(), 0));
        assert_eq!(demux_grundig(&[]).unwrap(), (Vec::<Vec<u8>>::new(), 0));

        let mut demuxer = GrundigSpStreamDemuxer::new(6 * BLOCK_SIZE);
        assert!(demuxer.push(&[0u8; 6 * BLOCK_SIZE]).unwrap().is_empty());
        assert!(demuxer.finish().unwrap().is_empty());
    }

    #[test]
    fn test_demux_grundig_refuses_a_second_part() {
        let with = |format: u8, count: u8, seed: u8| {
            let mut blk = block(0x00, 3, count, seed);
            blk[4] = format;
            blk
        };
        let as_grundig = |data: &[u8]| {
            let mut data = data.to_vec();
            for blk in data.chunks_mut(BLOCK_SIZE) {
                blk[4] = 0;
            }
            demux_grundig(&data).unwrap()
        };
        for format in [1u8, 2, 4, 6, 7] {
            let data = [with(0, 2, 1), with(0, 2, 2), with(format, 2, 3)].concat();
            assert!(
                matches!(
                    demux_grundig(&data),
                    Err(DecodeError::MultipleParts(f)) if f == format
                ),
                "0 then {format}"
            );
            let data = [with(0, 2, 1), with(format, 0, 2), with(0, 2, 3)].concat();
            assert_eq!(demux_grundig(&data).unwrap(), as_grundig(&data));
        }
        let data = [with(0, 2, 1), with(1, 0, 2), with(6, 2, 3)].concat();
        assert!(matches!(
            demux_grundig(&data),
            Err(DecodeError::MultipleParts(6))
        ));
        for unknown in [3u8, 5, 8, 255] {
            let data = [with(0, 2, 1), with(unknown, 2, 2), with(0, 2, 3)].concat();
            let bits = payload_bits(&data);
            let expected = [0, 328, 4048, 4376, 8096, 8424].map(|pos| frame_at(&bits, pos));
            assert_eq!(demux_grundig(&data).unwrap(), (expected.to_vec(), 6));
            assert_eq!(demux_grundig(&data).unwrap(), as_grundig(&data));
        }
    }
}
