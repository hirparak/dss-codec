/// DS2 file demuxer.
///
/// SP mode (0-1): DSS SP blocks, read by the DSS SP block walk.
/// QP mode (6): 56-byte frames, read from each block's anchor and across
/// block boundaries.
/// QP7 mode (7): the same, with 12-byte noise frames where the type bit is 0.
/// LP mode (2 or 4): G.723.1 frames, read by the DSS block walk.
use crate::demux::dss::{
    block_frames, block_start_bits, dss_single_part, header_format, part_format, TaggedFrames,
};
use crate::demux::{audio_start, check_sp_codec, ds2_header_size};
use crate::error::{DecodeError, Result};

const DS2_BLOCK_SIZE: usize = 512;
pub(crate) const DS2_BLOCK_HEADER_SIZE: usize = 6;
const DS2_QP_FRAME_SIZE: usize = 56;
const DS2_QP7_PACKET_SHORT_SIZE: usize = 12;
const DS2_QP7_TYPE_BYTES: usize = 2;
const DS2_BLOCK_PAYLOAD_SIZE: usize = DS2_BLOCK_SIZE - DS2_BLOCK_HEADER_SIZE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ds2QpSegment {
    pub stream: Vec<u8>,
    pub total_frames: usize,
    pub reset_before: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ds2Qp7Segment {
    pub records: Vec<Vec<u8>>,
    pub total_frames: usize,
    pub reset_before: bool,
}

/// The format of the first block with frames whose format counts, or where
/// no block's does, byte 4 of the first block with frames.
pub(crate) fn find_ds2_format_type(data: &[u8], header_size: usize) -> Option<u8> {
    let mut unknown = None;
    let mut bstart = header_size;
    while bstart + DS2_BLOCK_HEADER_SIZE <= data.len() {
        let hdr = &data[bstart..bstart + DS2_BLOCK_HEADER_SIZE];
        if block_frames(hdr) > 0 {
            let format = header_format(hdr);
            if part_format(format).is_some() {
                return Some(format);
            }
            unknown = unknown.or(Some(hdr[4]));
        }
        bstart += DS2_BLOCK_SIZE;
    }
    unknown
}

/// The format of the first block with frames whose format counts.
pub(crate) fn known_ds2_format_type(data: &[u8], header_size: usize) -> Option<u8> {
    let mut bstart = header_size;
    while bstart + DS2_BLOCK_HEADER_SIZE <= data.len() {
        let hdr = &data[bstart..bstart + DS2_BLOCK_HEADER_SIZE];
        let format = header_format(hdr);
        if block_frames(hdr) > 0 && part_format(format).is_some() {
            return Some(format);
        }
        bstart += DS2_BLOCK_SIZE;
    }
    None
}

pub(crate) fn detect_ds2_format_type(data: &[u8], header_size: usize) -> u8 {
    find_ds2_format_type(data, header_size).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ds2Layout {
    Single,
    Walk,
    MixedQp,
}

pub(crate) fn block_format(last: &mut Option<u8>, hdr: &[u8]) -> u8 {
    let format = header_format(hdr);
    if part_format(format).is_none() {
        return last.unwrap_or(format);
    }
    if block_frames(hdr) > 0 {
        *last = Some(format);
    }
    format
}

pub(crate) fn ds2_layout(data: &[u8], header_size: usize) -> Ds2Layout {
    let mut formats = 0u32;
    let mut last = known_ds2_format_type(data, header_size);
    let mut pos = header_size;
    while pos + DS2_BLOCK_HEADER_SIZE <= data.len() {
        let hdr = &data[pos..pos + DS2_BLOCK_HEADER_SIZE];
        let format = block_format(&mut last, hdr);
        if format < 8 && block_frames(hdr) > 0 {
            formats |= 1 << format;
        }
        pos += DS2_BLOCK_SIZE;
    }
    if formats & (1 << 0 | 1 << 1 | 1 << 2 | 1 << 4) != 0 {
        Ds2Layout::Walk
    } else if formats == (1 << 6 | 1 << 7) {
        Ds2Layout::MixedQp
    } else {
        Ds2Layout::Single
    }
}

/// Demux a DS2 file.
/// For SP: a Vec<Vec<u8>> of packets.
/// For QP/QP7: one segment holding every frame in file order.
/// For LP: a Vec<Vec<u8>> of G.723.1 frames.
pub fn demux_ds2(data: &[u8]) -> Result<DemuxedDs2> {
    let header_size = ds2_header_size(data)
        .map(|size| audio_start(data, size))
        .ok_or_else(|| DecodeError::NotDs2(std::path::PathBuf::from("<bytes>")))?;
    let format_type = detect_ds2_format_type(data, header_size);

    let tagged = if format_type == 6 || format_type == 7 {
        match ds2_layout(data, header_size) {
            Ds2Layout::Single => qp_frames(data, header_size, format_type == 7)
                .0
                .into_iter()
                .map(|frame| (format_type, frame))
                .collect(),
            Ds2Layout::Walk => dss_single_part(data, header_size, format_type)?.0,
            Ds2Layout::MixedQp => {
                let tagged = mixed_qp_frames(data, header_size).0;
                let first = tagged.first().map(|(tag, _)| *tag);
                if let Some((tag, _)) = tagged.iter().find(|(tag, _)| Some(*tag) != first) {
                    return Err(DecodeError::MultipleParts(*tag));
                }
                tagged
            }
        }
    } else {
        if part_format(format_type) != Some(2) {
            check_sp_codec(format_type)?;
        }
        dss_single_part(data, header_size, format_type)?.0
    };
    let kind = tagged.first().map_or(format_type, |(tag, _)| *tag);
    let frames: Vec<Vec<u8>> = tagged.into_iter().map(|(_, frame)| frame).collect();
    let total_frames = frames.len();
    if kind == 0 || kind == 1 {
        Ok(DemuxedDs2::Sp {
            total_frames,
            packets: frames,
        })
    } else if kind == 2 || kind == 4 {
        Ok(DemuxedDs2::Lp {
            total_frames,
            frames,
        })
    } else if kind == 7 {
        let segments = if frames.is_empty() {
            Vec::new()
        } else {
            vec![Ds2Qp7Segment {
                records: frames,
                total_frames,
                reset_before: false,
            }]
        };
        Ok(DemuxedDs2::Qp7Segments {
            segments,
            total_frames,
        })
    } else {
        let segments = if frames.is_empty() {
            Vec::new()
        } else {
            vec![Ds2QpSegment {
                stream: frames.concat(),
                total_frames,
                reset_before: false,
            }]
        };
        Ok(DemuxedDs2::QpSegments {
            segments,
            total_frames,
        })
    }
}

pub(crate) fn block_anchor(hdr: &[u8]) -> usize {
    (block_start_bits(hdr) >> 3)
        .saturating_sub(DS2_BLOCK_HEADER_SIZE)
        .min(DS2_BLOCK_PAYLOAD_SIZE)
}

fn qp_block_frames(hdr: &[u8], format: u8, mixed: bool) -> usize {
    if mixed && format != 6 && format != 7 {
        0
    } else {
        block_frames(hdr)
    }
}

fn block_frames_held(block: &[u8], format: u8, qp7: bool, mixed: bool) -> (usize, usize) {
    let size = block.len();
    let frame_count = qp_block_frames(block, format, mixed);
    let qp7 = if mixed { format == 7 } else { qp7 };
    let mut n = 0;
    let mut off = DS2_BLOCK_HEADER_SIZE + block_anchor(block);
    while n < frame_count && off < size {
        n += 1;
        if qp7 && off + 1 < size && block[off + 1] & 0x80 == 0 {
            off += DS2_QP7_PACKET_SHORT_SIZE;
        } else {
            off += DS2_QP_FRAME_SIZE;
        }
    }
    let need = if size == DS2_BLOCK_SIZE {
        off.saturating_sub(DS2_BLOCK_SIZE)
    } else {
        0
    };
    (n, need)
}

fn frame_dropped(data: &[u8], mut pos: usize, mut need: usize, format: u8, mixed: bool) -> bool {
    let mut last = Some(format);
    while need > 0 {
        pos += DS2_BLOCK_SIZE;
        let hdr = match data.get(pos..pos + DS2_BLOCK_HEADER_SIZE) {
            Some(hdr) => hdr,
            None => return false,
        };
        let format = block_format(&mut last, hdr);
        if qp_block_frames(hdr, format, mixed) != 0 {
            return block_anchor(hdr) != need;
        }
        need = need.saturating_sub(block_anchor(hdr));
    }
    false
}

fn count_qp_frames(data: &[u8], header_size: usize, qp7: bool, mixed: bool) -> usize {
    let mut total = 0;
    let mut last = known_ds2_format_type(data, header_size);
    let mut pos = header_size;
    while pos + DS2_BLOCK_HEADER_SIZE <= data.len() {
        let block = &data[pos..data.len().min(pos + DS2_BLOCK_SIZE)];
        let format = block_format(&mut last, block);
        let (n, need) = block_frames_held(block, format, qp7, mixed);
        total += if need > 0 && frame_dropped(data, pos, need, format, mixed) {
            n - 1
        } else {
            n
        };
        pos += DS2_BLOCK_SIZE;
    }
    total
}

struct QpFrameReader<'a> {
    data: &'a [u8],
    pos: usize,
    qp7: bool,
    mixed: bool,
    last_format: Option<u8>,
    block_format: u8,
    frame_format: u8,
    total_frames: usize,
    frames_read: usize,
    counter: usize,
    blk_frames: isize,
    frame_size: usize,
    resync_drop: bool,
    cut: bool,
}

impl<'a> QpFrameReader<'a> {
    fn new(data: &'a [u8], header_size: usize, qp7: bool, mixed: bool) -> Self {
        Self {
            data,
            pos: header_size,
            qp7,
            mixed,
            last_format: known_ds2_format_type(data, header_size),
            block_format: 0,
            frame_format: 0,
            total_frames: count_qp_frames(data, header_size, qp7, mixed),
            frames_read: 0,
            counter: 0,
            blk_frames: 0,
            frame_size: DS2_QP_FRAME_SIZE,
            resync_drop: false,
            cut: false,
        }
    }

    fn frame_qp7(&self) -> bool {
        if self.mixed {
            self.frame_format == 7
        } else {
            self.qp7
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> usize {
        let start = self.pos.min(self.data.len());
        let got = buf.len().min(self.data.len() - start);
        buf[..got].copy_from_slice(&self.data[start..start + got]);
        self.pos += got;
        got
    }

    fn all_read(&self) -> bool {
        self.total_frames > 0 && self.frames_read >= self.total_frames
    }

    fn next_block(&mut self, in_frame_offset: usize) -> bool {
        let in_block = self.pos % DS2_BLOCK_SIZE;
        self.counter = 0;
        self.resync_drop = false;
        if in_block > 0 {
            self.pos += DS2_BLOCK_SIZE - in_block;
        }
        if self.all_read() {
            return false;
        }

        let mut hdr = [0u8; DS2_BLOCK_HEADER_SIZE];
        if self.read(&mut hdr) != DS2_BLOCK_HEADER_SIZE {
            return false;
        }
        self.block_format = block_format(&mut self.last_format, &hdr);
        let frame_count = qp_block_frames(&hdr, self.block_format, self.mixed);
        let anchor = block_anchor(&hdr);
        self.blk_frames = frame_count as isize;

        if frame_count == 0 {
            self.counter = anchor;
            return true;
        }
        let aligned = (in_frame_offset == 0 && anchor == 0)
            || (in_frame_offset != 0 && in_frame_offset + anchor == self.frame_size);
        if aligned {
            self.counter = DS2_BLOCK_PAYLOAD_SIZE;
        } else {
            self.resync_drop = true;
            self.pos += anchor;
            self.counter = DS2_BLOCK_PAYLOAD_SIZE - anchor;
        }
        true
    }

    fn read_frame(&mut self) -> Option<Vec<u8>> {
        if self.all_read() {
            return None;
        }
        while self.blk_frames <= 0 || self.counter == 0 {
            if !self.next_block(0) {
                return None;
            }
        }

        let mut frame = [0u8; DS2_QP_FRAME_SIZE];
        let mut offset = 0;
        self.frame_size = DS2_QP_FRAME_SIZE;
        while offset < self.frame_size {
            let mut to_read = (self.frame_size - offset).min(self.counter);
            if to_read == 0 {
                if !self.next_block(offset) {
                    if offset > 0 && !self.resync_drop {
                        break;
                    }
                    return None;
                }
                if self.resync_drop {
                    offset = 0;
                    self.frame_size = DS2_QP_FRAME_SIZE;
                }
                to_read = (self.frame_size - offset).min(self.counter);
                if to_read == 0 {
                    continue;
                }
            }

            if offset == 0 {
                self.frame_format = self.block_format;
            }
            if self.frame_qp7() && offset == 0 {
                to_read = to_read.min(DS2_QP7_TYPE_BYTES);
            }
            let got = self.read(&mut frame[offset..offset + to_read]);
            if got < to_read {
                offset += got;
                if offset == 0 {
                    return None;
                }
                self.counter = 0;
                break;
            }

            if offset == 0 {
                self.blk_frames -= 1;
                if self.frame_qp7() && frame[1] & 0x80 == 0 {
                    self.frame_size = DS2_QP7_PACKET_SHORT_SIZE;
                }
            }
            offset += to_read;
            self.counter -= to_read;
        }

        if offset < self.frame_size {
            frame[offset..self.frame_size].fill(0);
            self.counter = 0;
            self.cut = true;
        }
        self.frames_read += 1;
        Some(frame[..self.frame_size].to_vec())
    }
}

fn qp_frames(data: &[u8], header_size: usize, qp7: bool) -> (Vec<Vec<u8>>, bool) {
    let mut reader = QpFrameReader::new(data, header_size, qp7, false);
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_frame() {
        frames.push(frame);
    }
    (frames, reader.cut)
}

pub(crate) fn mixed_qp_frames(data: &[u8], header_size: usize) -> (TaggedFrames, bool) {
    let mut reader = QpFrameReader::new(data, header_size, false, true);
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_frame() {
        let tag = if reader.frame_qp7() { 7 } else { 6 };
        frames.push((tag, frame));
    }
    (frames, reader.cut)
}

pub(crate) fn qp_frame_in_progress(data: &[u8]) -> bool {
    let Some(header_size) = ds2_header_size(data).map(|size| audio_start(data, size)) else {
        return false;
    };
    let tail = data.len().saturating_sub(header_size) % DS2_BLOCK_SIZE;
    if tail == 0 {
        return false;
    }
    let mixed = ds2_layout(data, header_size) == Ds2Layout::MixedQp;
    let qp7 = detect_ds2_format_type(data, header_size) == 7;
    let last = &data[data.len() - tail..];
    let mut last_format = known_ds2_format_type(data, header_size);
    let mut format = 0;
    let mut pos = header_size;
    while pos + DS2_BLOCK_HEADER_SIZE <= data.len() {
        format = block_format(&mut last_format, &data[pos..pos + DS2_BLOCK_HEADER_SIZE]);
        pos += DS2_BLOCK_SIZE;
    }
    if last.len() >= DS2_BLOCK_HEADER_SIZE
        && qp_block_frames(last, format, mixed) > block_frames_held(last, format, qp7, mixed).0
    {
        return true;
    }
    if mixed {
        mixed_qp_frames(data, header_size).1
    } else {
        qp_frames(data, header_size, qp7).1
    }
}

pub enum DemuxedDs2 {
    Sp {
        packets: Vec<Vec<u8>>,
        total_frames: usize,
    },
    QpSegments {
        segments: Vec<Ds2QpSegment>,
        total_frames: usize,
    },
    Qp7Segments {
        segments: Vec<Ds2Qp7Segment>,
        total_frames: usize,
    },
    Lp {
        frames: Vec<Vec<u8>>,
        total_frames: usize,
    },
}

pub(crate) struct Ds2QpStreamDemuxer {
    data: Vec<u8>,
}

impl Ds2QpStreamDemuxer {
    pub(crate) fn new() -> Self {
        Self { data: Vec::new() }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.data.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        self.finish_lenient()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        match demux_ds2(&self.data)? {
            DemuxedDs2::QpSegments {
                segments,
                total_frames: _,
            } => Ok(segments.into_iter().flat_map(|seg| {
                seg.stream
                    .chunks(DS2_QP_FRAME_SIZE)
                    .take(seg.total_frames)
                    .map(|chunk| chunk.to_vec())
                    .collect::<Vec<_>>()
            }).collect()),
            DemuxedDs2::Qp7Segments {
                segments,
                total_frames: _,
            } => Ok(segments.into_iter().flat_map(|seg| seg.records).collect()),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_file(mode: u8, frame_count: u8, payload_pattern: u8) -> Vec<u8> {
        make_ds2_file_with_header(3, mode, frame_count, payload_pattern)
    }

    fn make_ds2_file_with_header(
        first_byte: u8,
        mode: u8,
        frame_count: u8,
        payload_pattern: u8,
    ) -> Vec<u8> {
        let header_blocks = if first_byte == 1 { 3 } else { first_byte as usize };
        let mut data = vec![0u8; header_blocks * DS2_BLOCK_SIZE];
        data[0] = first_byte;
        data[1..4].copy_from_slice(b"ds2");

        let mut block = [0u8; DS2_BLOCK_SIZE];
        block[1] = 3;
        block[2] = frame_count;
        block[3] = 0xff;
        block[4] = mode;
        for (i, byte) in block[DS2_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }

        data.extend_from_slice(&block);
        data
    }

    fn sp_packets(data: &[u8]) -> Vec<Vec<u8>> {
        match demux_ds2(data).unwrap() {
            DemuxedDs2::Sp {
                packets,
                total_frames,
            } => {
                assert_eq!(total_frames, packets.len());
                packets
            }
            _ => panic!("expected DS2 SP packets"),
        }
    }

    #[test]
    fn test_ds2_sp_demux_reads_dss_blocks() {
        let data = make_ds2_file(0, 4, 0x10);
        let mut dss = vec![0u8; 2 * DS2_BLOCK_SIZE];
        dss[..4].copy_from_slice(b"\x02dss");
        dss.extend_from_slice(&data[3 * DS2_BLOCK_SIZE..]);

        let (expected, _) = crate::demux::dss::demux_dss(&dss).unwrap();
        assert_eq!(expected.len(), 4);
        assert_eq!(sp_packets(&data), expected);
    }

    #[test]
    fn test_ds2_sp_demux_overdeclared_frames_stop_at_end_of_file() {
        let data = make_ds2_file(0, 13, 0x20);
        let packets = sp_packets(&data);
        assert_eq!(packets.len(), 12);
        assert!(packets.iter().all(|p| p.len() == 42));
    }

    #[test]
    fn test_ds2_variable_rate_demux_matches_dss_twin() {
        let data = make_ds2_file(1, 4, 0x70);
        let mut dss = vec![0u8; 2 * DS2_BLOCK_SIZE];
        dss[..4].copy_from_slice(b"\x02dss");
        dss.extend_from_slice(&data[3 * DS2_BLOCK_SIZE..]);

        let (expected, _) = crate::demux::dss::demux_dss(&dss).unwrap();
        assert_eq!(expected.len(), 4);
        assert_eq!(sp_packets(&data), expected);
        assert_ne!(sp_packets(&data), sp_packets(&make_ds2_file(0, 4, 0x70)));
    }

    #[test]
    fn test_ds2_sp_demux_two_block_header() {
        let data = make_ds2_file_with_header(2, 0, 4, 0x10);
        let short = make_ds2_file(0, 4, 0x10);
        assert_eq!(sp_packets(&data).len(), 4);
        assert_eq!(sp_packets(&data), sp_packets(&short));
    }

    #[test]
    fn test_find_ds2_format_type_reads_first_block_with_frames() {
        let mut data = make_ds2_file(6, 0, 0x10);
        let second = make_ds2_file(7, 2, 0x10);
        data.extend_from_slice(&second[3 * DS2_BLOCK_SIZE..]);
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(7));
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 7);
    }

    #[test]
    fn test_find_ds2_format_type_reads_partial_block_header() {
        let data = make_ds2_file(6, 2, 0x10);
        let cut = 3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE;
        assert_eq!(find_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), Some(6));
        assert_eq!(detect_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), 6);
    }

    #[test]
    fn test_find_ds2_format_type_none_without_block_header() {
        let data = make_ds2_file(6, 2, 0x10);
        let cut = 3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE - 1;
        assert_eq!(find_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), None);
        assert_eq!(detect_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), 0);
        assert_eq!(find_ds2_format_type(&data[..100], 3 * DS2_BLOCK_SIZE), None);
    }

    #[test]
    fn test_find_ds2_format_type_without_a_qualifying_block() {
        let mut data = make_ds2_file(7, 2, 0x10);
        let second = make_ds2_file(6, 2, 0x10);
        data.extend_from_slice(&second[3 * DS2_BLOCK_SIZE..]);
        data[3 * DS2_BLOCK_SIZE + 3] = 0;
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(6));
        assert_eq!(known_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(6));

        data[4 * DS2_BLOCK_SIZE + 3] = 0;
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(7));
        assert_eq!(known_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), None);
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 7);

        data[3 * DS2_BLOCK_SIZE + 4] = 5;
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(5));
        assert_eq!(known_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), None);
    }

    #[test]
    fn test_find_ds2_format_type_none_when_no_block_has_frames() {
        let mut data = make_ds2_file(6, 0, 0x10);
        let second = make_ds2_file(7, 0, 0x10);
        data.extend_from_slice(&second[3 * DS2_BLOCK_SIZE..]);
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), None);
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 0);
    }

    #[test]
    fn test_ds2_demux_refuses_unknown_codecs() {
        for mode in [3u8, 5, 8, 255] {
            let data = make_ds2_file(mode, 4, 0x10);
            assert!(matches!(
                demux_ds2(&data),
                Err(DecodeError::UnsupportedCodec(m)) if m == mode
            ));
        }
    }

    #[test]
    fn test_ds2_demux_reads_lp_blocks() {
        let payload = &make_ds2_file(2, 4, 0x10)[3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE..];
        let expected: Vec<Vec<u8>> = payload.chunks(24).take(4).map(|f| f.to_vec()).collect();
        for mode in [2u8, 4] {
            assert_eq!(lp_frames_of(&make_ds2_file(mode, 4, 0x10)), expected, "{mode}");
        }
    }

    #[test]
    fn test_ds2_qp_stream_demux_matches_batch() {
        let data = make_ds2_file(6, 3, 0x40);
        let expected = match demux_ds2(&data).unwrap() {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => segments
                .into_iter()
                .flat_map(|seg| seg.stream.into_iter())
                .collect::<Vec<_>>()[..total_frames * DS2_QP_FRAME_SIZE]
                .chunks(DS2_QP_FRAME_SIZE)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
            _ => panic!("expected DS2 QP stream"),
        };

        let mut demuxer = Ds2QpStreamDemuxer::new();
        let mut actual = Vec::new();
        for chunk in data.chunks(113) {
            actual.extend(demuxer.push(chunk).unwrap());
        }
        actual.extend(demuxer.finish().unwrap());

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ds2_qp_demux_two_block_header() {
        let data = make_ds2_file_with_header(2, 6, 3, 0x40);
        let short = make_ds2_file(6, 3, 0x40);
        let frames = |demuxed: DemuxedDs2| match demuxed {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => (segments, total_frames),
            _ => panic!("expected DS2 QP stream"),
        };
        assert_eq!(frames(demux_ds2(&data).unwrap()), frames(demux_ds2(&short).unwrap()));
    }

    #[test]
    fn test_ds2_demux_rejects_bad_header_byte() {
        let mut data = make_ds2_file(6, 3, 0x40);
        data[0] = 0;
        assert!(matches!(demux_ds2(&data), Err(DecodeError::NotDs2(_))));
    }

    #[test]
    fn test_ds2_qp_demux_one_block_header() {
        let mut long = make_ds2_file(6, 3, 0x40);
        long[3 * DS2_BLOCK_SIZE + 3] = 0xff;
        let mut data = long[..DS2_BLOCK_SIZE].to_vec();
        data[0] = 1;
        data.extend_from_slice(&long[3 * DS2_BLOCK_SIZE..]);
        let frames = |demuxed: DemuxedDs2| match demuxed {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => (segments, total_frames),
            _ => panic!("expected DS2 QP stream"),
        };
        assert_eq!(frames(demux_ds2(&data).unwrap()), frames(demux_ds2(&long).unwrap()));
    }

    fn qp_block(b1: u8, b2: u8, b5: u8, mode: u8, payload: &[u8]) -> Vec<u8> {
        let mut block = vec![0u8; DS2_BLOCK_SIZE];
        block[1] = b1;
        block[2] = b2;
        block[3] = 0xff;
        block[4] = mode;
        block[5] = b5;
        block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_HEADER_SIZE + payload.len()].copy_from_slice(payload);
        block
    }

    fn qp_file(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut data = vec![0u8; 3 * DS2_BLOCK_SIZE];
        data[0] = 3;
        data[1..4].copy_from_slice(b"ds2");
        for block in blocks {
            data.extend_from_slice(block);
        }
        data
    }

    fn stream_bytes(start: usize, len: usize) -> Vec<u8> {
        (start..start + len).map(|i| (i % 251) as u8 + 1).collect()
    }

    fn anchor_byte(anchor: usize) -> u8 {
        ((anchor + DS2_BLOCK_HEADER_SIZE) / 2) as u8
    }

    fn qp_frames_of(data: &[u8]) -> Vec<Vec<u8>> {
        match demux_ds2(data).unwrap() {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => {
                assert!(segments.iter().all(|seg| !seg.reset_before));
                let frames: Vec<Vec<u8>> = segments
                    .iter()
                    .flat_map(|seg| seg.stream.chunks(DS2_QP_FRAME_SIZE).map(|c| c.to_vec()))
                    .collect();
                assert_eq!(frames.len(), total_frames);
                frames
            }
            DemuxedDs2::Qp7Segments {
                segments,
                total_frames,
            } => {
                assert!(segments.iter().all(|seg| !seg.reset_before));
                let frames: Vec<Vec<u8>> =
                    segments.into_iter().flat_map(|seg| seg.records).collect();
                assert_eq!(frames.len(), total_frames);
                frames
            }
            _ => panic!("expected DS2 QP frames"),
        }
    }

    fn stream_frames(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|k| stream_bytes(k * DS2_QP_FRAME_SIZE, DS2_QP_FRAME_SIZE))
            .collect()
    }

    fn zero_filled(start: usize, len: usize) -> Vec<u8> {
        let mut frame = stream_bytes(start, len);
        frame.resize(DS2_QP_FRAME_SIZE, 0);
        frame
    }

    fn lp_frames_of(data: &[u8]) -> Vec<Vec<u8>> {
        match demux_ds2(data).unwrap() {
            DemuxedDs2::Lp {
                frames,
                total_frames,
            } => {
                assert_eq!(frames.len(), total_frames);
                frames
            }
            _ => panic!("expected DS2 LP frames"),
        }
    }

    /// G.723.1 frames of the given sizes, back to back, each with the type
    /// bits that give its size.
    fn lp_frames(sizes: &[usize]) -> Vec<Vec<u8>> {
        let mut at = 0;
        sizes
            .iter()
            .map(|&size| {
                let mut frame = stream_bytes(at, size);
                at += size;
                let kind = match size {
                    24 => 0,
                    20 => 1,
                    4 => 2,
                    _ => 3,
                };
                frame[0] = frame[0] & !3 | kind;
                frame
            })
            .collect()
    }

    #[test]
    fn test_ds2_lp_type_bits_size_frames() {
        let frames = lp_frames(&[24, 20, 4, 1, 24, 1]);
        let data = qp_file(&[qp_block(anchor_byte(0), 6, 0xff, 2, &frames.concat())]);
        assert_eq!(lp_frames_of(&data), frames);
    }

    #[test]
    fn test_ds2_lp_frames_continue_across_blocks() {
        let frames = lp_frames(&[24; 42]);
        let stream = frames.concat();
        let data = qp_file(&[
            qp_block(anchor_byte(0), 22, 0xff, 2, &stream[..506]),
            qp_block(anchor_byte(22), 20, 0xff, 2, &stream[506..]),
        ]);
        assert_eq!(lp_frames_of(&data), frames);
    }

    #[test]
    fn test_ds2_lp_resync_block_drops_partial_frame() {
        let frames = lp_frames(&[24; 43]);
        let stream = frames.concat();
        let mut second = vec![0u8; 30];
        second.extend_from_slice(&stream[528..]);
        let data = qp_file(&[
            qp_block(anchor_byte(0), 22, 0xff, 2, &stream[..506]),
            qp_block(anchor_byte(30), 19, 0xff, 2, &second[..506]),
        ]);
        let mut expected = frames[..21].to_vec();
        expected.extend_from_slice(&frames[22..41]);
        assert_eq!(lp_frames_of(&data), expected);
    }

    #[test]
    fn test_ds2_lp_pause_block_frames_from_byte5() {
        let frames = lp_frames(&[24, 4, 1, 1, 20]);
        let data = qp_file(&[qp_block(anchor_byte(0), 0, 5, 2, &frames.concat())]);
        assert_eq!(lp_frames_of(&data), frames);
    }

    #[test]
    fn test_ds2_lp_file_ending_inside_frame_zero_fills_it() {
        let frames = lp_frames(&[24, 24]);
        let data = qp_file(&[qp_block(anchor_byte(0), 2, 0xff, 2, &frames.concat())]);
        let cut = &data[..3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE + 30];
        let mut last = frames[1][..6].to_vec();
        last.resize(24, 0);
        assert_eq!(lp_frames_of(cut), vec![frames[0].clone(), last]);
    }

    #[test]
    fn test_ds2_qp_frames_continue_across_blocks() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 8, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        assert_eq!(qp_frames_of(&data), stream_frames(18));
    }

    #[test]
    fn test_ds2_qp_pause_block_frames_from_byte5() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 0, 3, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(0), 0, 0xff, 6, &stream_bytes(1000, 506)),
            qp_block(anchor_byte(0), 1, 0xff, 6, &stream_bytes(168, 506)),
        ]);
        assert_eq!(qp_frames_of(&data), stream_frames(4));
    }

    #[test]
    fn test_ds2_qp_pause_block_sets_format() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 0, 0xff, 7, &stream_bytes(0, 506)),
            qp_block(anchor_byte(0), 0, 3, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(0), 1, 0xff, 7, &stream_bytes(1000, 506)),
        ]);
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(6));
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 6);

        let pause = qp_file(&[qp_block(anchor_byte(0), 0, 3, 6, &stream_bytes(0, 506))]);
        let mut counted = pause.clone();
        counted[3 * DS2_BLOCK_SIZE + 2] = 3;
        counted[3 * DS2_BLOCK_SIZE + 5] = 0xff;
        assert_eq!(find_ds2_format_type(&pause, 3 * DS2_BLOCK_SIZE), Some(6));
        assert_eq!(qp_frames_of(&pause), stream_frames(3));
        assert_eq!(qp_frames_of(&pause), qp_frames_of(&counted));
    }

    #[test]
    fn test_block_anchor_reads_byte3_when_pointer_is_0() {
        for (header, anchor) in [
            ([0x0f, 0x05, 0x09, 0xff, 0x06, 0xff], 4),
            ([0x0f, 0x05, 0x09, 0x40, 0x06, 0xff], 4),
            ([0x01, 0x00, 0x00, 0xc0, 0x06, 0x09], 50),
            ([0x00, 0x00, 0x00, 0x40, 0x06, 0x09], 2),
            ([0x03, 0x00, 0x00, 0x28, 0x00, 0x0b], 95),
            ([0x00, 0x00, 0x00, 0x00, 0x06, 0x09], 0),
            ([0x00, 0x00, 0x00, 0x20, 0x06, 0x09], 0),
            ([0x0f, 0xff, 0x00, 0xff, 0x06, 0xff], 504),
        ] {
            assert_eq!(block_anchor(&header), anchor, "{header:02x?}");
        }
    }

    #[test]
    fn test_ds2_qp_pause_block_frames_at_its_pointer() {
        let mut pause = qp_block(0, 0, 9, 6, &stream_bytes(506, 506));
        pause[..4].copy_from_slice(&[0x01, 0x00, 0x00, 0xe0]);
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            pause,
            qp_block(anchor_byte(52), 1, 0xff, 6, &stream_bytes(1012, 506)),
        ]);
        assert_eq!(qp_frames_of(&data), stream_frames(20));

        let mut olympus = data.clone();
        olympus[4 * DS2_BLOCK_SIZE..4 * DS2_BLOCK_SIZE + 6]
            .copy_from_slice(&[0x00, anchor_byte(54), 9, 0xff, 6, 0xff]);
        assert_eq!(qp_frames_of(&data), qp_frames_of(&olympus));
    }

    #[test]
    fn test_ds2_qp_overstated_count_stops_at_block_end() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 13, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 1, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        assert_eq!(qp_frames_of(&data), stream_frames(11));
    }

    #[test]
    fn test_ds2_qp_block_over_13_frames_is_empty() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 19, 0xff, 6, &stream_bytes(506, 506)),
            qp_block(anchor_byte(48), 1, 0xff, 6, &stream_bytes(1012, 506)),
        ]);
        let mut expected = stream_frames(10);
        expected.push(stream_bytes(1012 + 48, DS2_QP_FRAME_SIZE));
        assert_eq!(qp_frames_of(&data), expected);

        let mut qp7 = data.clone();
        for block in 0..3 {
            qp7[(3 + block) * DS2_BLOCK_SIZE + 4] = 7;
        }
        assert!(qp_frames_of(&qp7).len() > expected.len());
    }

    #[test]
    fn test_ds2_qp_empty_block_carries_frame_tail() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(30), 0, 0xff, 6, &stream_bytes(506, 506)),
            qp_block(anchor_byte(24), 1, 0xff, 6, &stream_bytes(536, 506)),
        ]);
        assert_eq!(qp_frames_of(&data), stream_frames(11));
    }

    #[test]
    fn test_ds2_qp7_type_bit_sizes_frames() {
        let mut payload = stream_bytes(0, 506);
        payload[1] &= 0x7f;
        payload[13] |= 0x80;
        payload[69] &= 0x7f;
        payload[81] |= 0x80;
        let data = qp_file(&[qp_block(anchor_byte(0), 4, 0xff, 7, &payload)]);
        assert_eq!(
            qp_frames_of(&data),
            vec![
                payload[..12].to_vec(),
                payload[12..68].to_vec(),
                payload[68..80].to_vec(),
                payload[80..136].to_vec(),
            ]
        );
    }

    #[test]
    fn test_ds2_qp_file_ending_inside_frame_zero_fills_it() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 9, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        let mut expected = stream_frames(18);
        expected.push(zero_filled(18 * DS2_QP_FRAME_SIZE, 4));
        assert_eq!(qp_frames_of(&data), expected);
    }

    #[test]
    fn test_ds2_qp_cut_block_counts_frames_that_start_in_it() {
        let data = qp_file(&[qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506))]);
        let cut = &data[..3 * DS2_BLOCK_SIZE + 300];
        let mut expected = stream_frames(5);
        expected.push(zero_filled(5 * DS2_QP_FRAME_SIZE, 14));
        assert_eq!(qp_frames_of(cut), expected);
    }

    #[test]
    fn test_ds2_qp_resync_block_drops_partial_frame() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(100), 7, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        let mut expected = stream_frames(9);
        expected.extend((0..7).map(|k| stream_bytes(606 + k * DS2_QP_FRAME_SIZE, DS2_QP_FRAME_SIZE)));
        assert_eq!(qp_frames_of(&data), expected);
    }

    #[test]
    fn test_ds2_qp_empty_block_with_wrong_anchor_drops_frame() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(30), 0, 0xff, 6, &stream_bytes(506, 506)),
            qp_block(anchor_byte(30), 1, 0xff, 6, &stream_bytes(536, 506)),
        ]);
        let mut expected = stream_frames(9);
        expected.push(stream_bytes(566, DS2_QP_FRAME_SIZE));
        assert_eq!(qp_frames_of(&data), expected);
    }

    #[test]
    fn test_ds2_qp7_cut_inside_type_bytes_keeps_zeroed_frame() {
        let mut payload = stream_bytes(0, 506);
        payload[1] &= 0x7f;
        payload[13] |= 0x80;
        let data = qp_file(&[qp_block(anchor_byte(0), 2, 0xff, 7, &payload)]);
        let cut = &data[..3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE + 13];
        let mut second = vec![0u8; DS2_QP_FRAME_SIZE];
        second[0] = payload[12];
        assert_eq!(qp_frames_of(cut), vec![payload[..12].to_vec(), second]);
    }

    #[test]
    fn test_ds2_qp_stops_at_counted_frames_and_partial_header() {
        let mut data = qp_file(&[qp_block(anchor_byte(0), 3, 0xff, 6, &stream_bytes(0, 506))]);
        data.extend_from_slice(&[0, anchor_byte(0), 5, 0]);
        assert_eq!(qp_frames_of(&data), stream_frames(3));
    }

    #[test]
    fn test_ds2_qp_frame_in_progress_false_without_one() {
        let spill = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 9, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        assert!(!qp_frame_in_progress(&spill));

        let data = qp_file(&[
            qp_block(anchor_byte(0), 9, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(0), 3, 0xff, 6, &stream_bytes(504, 506)),
        ]);
        let second = 4 * DS2_BLOCK_SIZE;
        for end in [
            second + 1,
            second + DS2_BLOCK_HEADER_SIZE - 1,
            second + DS2_BLOCK_HEADER_SIZE + 3 * DS2_QP_FRAME_SIZE,
            second + 300,
            data.len(),
        ] {
            assert!(!qp_frame_in_progress(&data[..end]), "end {end}");
        }
        assert!(!qp_frame_in_progress(&data[..3 * DS2_BLOCK_SIZE - 1]));
    }

    #[test]
    fn test_ds2_qp_frame_in_progress_inside_cut_block() {
        let data = qp_file(&[qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506))]);
        let first = 3 * DS2_BLOCK_SIZE;
        for end in [
            first + DS2_BLOCK_HEADER_SIZE,
            first + DS2_BLOCK_HEADER_SIZE + 5 * DS2_QP_FRAME_SIZE,
            first + 300,
            first + DS2_BLOCK_SIZE - 1,
        ] {
            assert!(qp_frame_in_progress(&data[..end]), "end {end}");
        }

        let mut payload = stream_bytes(0, 506);
        payload[1] &= 0x7f;
        payload[13] |= 0x80;
        let qp7 = qp_file(&[qp_block(anchor_byte(0), 2, 0xff, 7, &payload)]);
        let type_byte = first + DS2_BLOCK_HEADER_SIZE + 13;
        assert!(qp_frame_in_progress(&qp7[..type_byte]));
        assert!(!qp_frame_in_progress(&qp7[..type_byte + 55]));
    }

    #[test]
    fn test_ds2_qp_frame_in_progress_from_earlier_block() {
        let data = qp_file(&[
            qp_block(anchor_byte(0), 10, 0xff, 6, &stream_bytes(0, 506)),
            qp_block(anchor_byte(54), 8, 0xff, 6, &stream_bytes(506, 506)),
        ]);
        let second = 4 * DS2_BLOCK_SIZE;
        assert!(!qp_frame_in_progress(&data[..second]));
        for end in [
            second + 1,
            second + DS2_BLOCK_HEADER_SIZE - 1,
            second + DS2_BLOCK_HEADER_SIZE,
            second + DS2_BLOCK_HEADER_SIZE + 53,
        ] {
            assert!(qp_frame_in_progress(&data[..end]), "end {end}");
            assert_eq!(qp_frames_of(&data[..end]).len(), 10);
        }
    }

    #[test]
    fn test_ds2_qp_stream_demux_lenient_batching() {
        let data = make_ds2_file(6, 3, 0x55);
        let mut demuxer = Ds2QpStreamDemuxer::new();
        for chunk in data.chunks(97) {
            let _ = demuxer.push(chunk).unwrap();
        }

        assert_eq!(demuxer.finish().unwrap().len(), 3);
    }
}
