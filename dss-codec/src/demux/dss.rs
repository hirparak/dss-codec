/// DSS SP demuxer.
///
/// Walks the file block by block. Each block header gives the offset of its
/// first frame, the byte-swap parity of that frame and the number of frames
/// that start in the block. Frames are read through the block headers they
/// straddle. A frame that the file does not hold in full ends the stream.
use crate::demux::ds2::block_anchor;
use crate::error::{DecodeError, Result};

const DSS_BLOCK_SIZE: usize = 512;
const DSS_BLOCK_HEADER_SIZE: usize = 6;
const DSS_BLOCK_PAYLOAD_SIZE: usize = DSS_BLOCK_SIZE - DSS_BLOCK_HEADER_SIZE;
const DSS_SP_FRAME_SIZE: usize = 42;
const DSS_SP_SHORT_FRAME_SIZE: usize = 9;
const DSS_SP_PACKET_BUF: usize = 48;
const DSS_QP_FRAME_SIZE: usize = 56;
const DSS_QP7_NOISE_FRAME_SIZE: usize = 12;
const BLOCK_MAX_FRAMES: u8 = 13;

pub(crate) type TaggedFrames = Vec<(u8, Vec<u8>)>;

/// Frames that start in a block: byte 2, or byte 5 when byte 2 is 0 or 0xff,
/// and none when that is 0xff. A block that states more than 13 holds none,
/// which drops the DS-2600's splice blocks that state 19, except where the
/// block's format counts, 0xff in byte 3, and is variable rate SP, LP or QP7,
/// whose frames can be short enough for more to start in a block.
pub(crate) fn block_frames(header: &[u8]) -> usize {
    let count = if header[2] != 0 && header[2] != 0xff {
        header[2]
    } else {
        header[5]
    };
    if count == 0xff
        || (count > BLOCK_MAX_FRAMES && !(header[3] == 0xff && matches!(header[4], 1 | 2 | 4 | 7)))
    {
        0
    } else {
        count as usize
    }
}

/// Where the first frame that starts in a block starts, in bits from the
/// block start: byte 1 and the top half of byte 0, or where both are 0, as in
/// Philips's pause blocks, the bottom half of byte 0 and byte 3. The first
/// frame starts on a whole byte, so this drops the low 3 bits of either.
pub(crate) fn block_start_bits(header: &[u8]) -> usize {
    let bits = usize::from(header[1]) << 4 | usize::from(header[0] >> 4);
    let bits = if bits != 0 {
        bits
    } else {
        usize::from(header[0] & 0x0f) << 8 | usize::from(header[3])
    };
    bits & !7
}

pub(crate) fn header_format(header: &[u8]) -> u8 {
    if header[3] == 0xff {
        header[4]
    } else {
        0xff
    }
}

pub(crate) fn part_format(format: u8) -> Option<u8> {
    match format {
        0 | 1 | 2 | 6 => Some(format),
        4 => Some(2),
        7 => Some(6),
        _ => None,
    }
}

pub fn demux_dss(data: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    let header_size = crate::demux::dss_header_size(data)
        .map(|size| crate::demux::audio_start(data, size))
        .ok_or_else(|| DecodeError::NotDss(std::path::PathBuf::from("<bytes>")))?;
    if data.len() < header_size {
        return Err(DecodeError::Truncated("DSS header".to_string()));
    }
    let codec = crate::demux::ds2::detect_ds2_format_type(data, header_size);
    crate::demux::check_sp_codec(codec)?;
    let (frames, _cut) = dss_single_part(data, header_size, codec)?;
    if let Some(&(tag, _)) = frames.first().filter(|(tag, _)| *tag != 0 && *tag != 1) {
        return Err(DecodeError::UnsupportedCodec(tag));
    }
    let frames: Vec<Vec<u8>> = frames.into_iter().map(|(_, frame)| frame).collect();
    let total_frames = frames.len();
    Ok((frames, total_frames))
}

pub(crate) fn dss_single_part(
    data: &[u8],
    header_size: usize,
    format: u8,
) -> Result<(TaggedFrames, bool)> {
    let mut walker = DssSpWalker::new(header_size, format);
    let src = DssBytes {
        data,
        base: 0,
        eof: true,
    };
    let mut frames: TaggedFrames = Vec::new();
    loop {
        match walker.step(&src) {
            DssStep::Frame(tag, frame) if frames.first().is_none_or(|(first, _)| *first == tag) => {
                frames.push((tag, frame))
            }
            DssStep::Frame(tag, _) => return Err(DecodeError::MultipleParts(tag)),
            DssStep::Unsupported(raw) => return Err(DecodeError::MultipleParts(raw)),
            DssStep::NeedMore => return Ok((frames, false)),
            DssStep::Dropped => {}
            DssStep::End { cut } => return Ok((frames, cut)),
        }
    }
}

struct DssBytes<'a> {
    data: &'a [u8],
    base: usize,
    eof: bool,
}

enum Short {
    NeedMore,
    End,
    PartEnd(u8),
}

impl DssBytes<'_> {
    fn get(&self, pos: usize, len: usize) -> std::result::Result<&[u8], Short> {
        let start = pos.checked_sub(self.base).ok_or(Short::End)?;
        if start + len <= self.data.len() {
            Ok(&self.data[start..start + len])
        } else if self.eof {
            Err(Short::End)
        } else {
            Err(Short::NeedMore)
        }
    }

    fn avail(&self, pos: usize, len: usize) -> std::result::Result<&[u8], Short> {
        match self.get(pos, len) {
            Err(Short::End) => {
                let start = pos.checked_sub(self.base).ok_or(Short::End)?;
                Ok(&self.data[start.min(self.data.len())..])
            }
            other => other,
        }
    }

    fn byte_or_zero(&self, pos: usize) -> std::result::Result<u8, Short> {
        match self.get(pos, 1) {
            Ok(bytes) => Ok(bytes[0]),
            Err(Short::End) => Ok(0),
            Err(other) => Err(other),
        }
    }
}

enum DssStep {
    Frame(u8, Vec<u8>),
    Dropped,
    NeedMore,
    Unsupported(u8),
    End { cut: bool },
}

#[derive(Clone, Copy)]
struct DssSpWalker {
    header_size: usize,
    format: u8,
    block_pos: usize,
    pos: usize,
    frames_left: usize,
    swap: bool,
    swap_byte: Option<u8>,
    qp_left: usize,
    qp_frame_size: usize,
    qp_block_format: u8,
    cut: bool,
    unsupported: Option<u8>,
}

impl DssSpWalker {
    fn new(header_size: usize, format: u8) -> Self {
        Self {
            header_size,
            format: part_format(format).unwrap_or(format),
            block_pos: header_size - DSS_BLOCK_SIZE,
            pos: header_size,
            frames_left: 0,
            swap: false,
            swap_byte: None,
            qp_left: 0,
            qp_frame_size: DSS_QP_FRAME_SIZE,
            qp_block_format: format,
            cut: false,
            unsupported: None,
        }
    }

    fn step(&mut self, src: &DssBytes) -> DssStep {
        loop {
            if let Some(raw) = self.unsupported {
                return DssStep::Unsupported(raw);
            }
            let saved = *self;
            match self.read_frame(src) {
                Ok(step) => return step,
                Err(Short::PartEnd(raw)) => self.set_part(raw),
                Err(_) => {
                    *self = saved;
                    return DssStep::NeedMore;
                }
            }
        }
    }

    fn set_part(&mut self, raw: u8) {
        self.format = part_format(raw).unwrap_or(raw);
        self.frames_left = 0;
        self.swap = false;
        self.swap_byte = None;
        if self.format == 2 {
            self.unsupported = Some(raw);
        }
    }

    fn other_format(&self, header: &[u8]) -> bool {
        block_frames(header) > 0
            && part_format(header_format(header)).is_some_and(|format| format != self.format)
    }

    fn read_frame(&mut self, src: &DssBytes) -> std::result::Result<DssStep, Short> {
        if self.format == 6 {
            return self.read_qp_frame(src);
        }
        if self.frames_left == 0 {
            match self.next_block(src) {
                Err(Short::End) => return Ok(DssStep::End { cut: false }),
                other => other?,
            }
        }

        let mut size = DSS_SP_FRAME_SIZE - 1;
        let mut done = 0;
        let mut pkt = [0u8; DSS_SP_PACKET_BUF];
        if self.format == 1 {
            if self.swap {
                if self.swap_byte.is_some_and(|byte| byte & 0x80 == 0) {
                    size = DSS_SP_SHORT_FRAME_SIZE;
                }
            } else {
                match self.read_payload(src, &mut pkt[..2]) {
                    Err(Short::End) => return Ok(DssStep::End { cut: true }),
                    other => other?,
                }
                done = 2;
                if pkt[1] & 0x80 == 0 {
                    size = DSS_SP_SHORT_FRAME_SIZE;
                }
            }
        }

        let read = if self.swap {
            self.read_payload(src, &mut pkt[3..3 + size - 1])
        } else {
            self.read_payload(src, &mut pkt[done..size + 1])
        };
        match read {
            Err(Short::End) => return Ok(DssStep::End { cut: true }),
            other => other?,
        }
        self.frames_left -= 1;

        self.byte_swap(&mut pkt, size);
        if self.swap_byte.is_none() {
            return Ok(DssStep::Dropped);
        }
        Ok(DssStep::Frame(self.format, pkt[..DSS_SP_FRAME_SIZE].to_vec()))
    }

    fn next_block(&mut self, src: &DssBytes) -> std::result::Result<(), Short> {
        loop {
            self.block_pos += DSS_BLOCK_SIZE;
            let header = src.get(self.block_pos, DSS_BLOCK_HEADER_SIZE)?;
            if self.other_format(header) {
                let raw = header[4];
                self.block_pos -= DSS_BLOCK_SIZE;
                return Err(Short::PartEnd(raw));
            }

            let start = (block_start_bits(header) >> 3) as isize;
            self.swap = start & 1 != 0;
            let mut cont = start + self.swap as isize - DSS_BLOCK_HEADER_SIZE as isize;
            if (cont < 0 && !self.swap) || cont > (DSS_BLOCK_SIZE - DSS_BLOCK_HEADER_SIZE) as isize
            {
                cont = 0;
            }

            self.frames_left = block_frames(header);
            if self.frames_left == 0 {
                continue;
            }

            let payload = self.block_pos + DSS_BLOCK_HEADER_SIZE;
            self.pos = if cont < 0 {
                self.block_pos - cont.unsigned_abs()
            } else {
                payload + cont as usize
            };
            if self.swap {
                let mut at = self.block_pos as isize + cont - 2;
                if cont >= 2 {
                    at += DSS_BLOCK_HEADER_SIZE as isize;
                }
                if at >= (self.header_size + DSS_BLOCK_HEADER_SIZE) as isize {
                    self.swap_byte = Some(src.byte_or_zero(at as usize)?);
                } else if cont < 0 {
                    let mut bits = 8 * (cont - 1);
                    while bits < 0 && self.frames_left > 0 {
                        bits += 8 * (DSS_SP_FRAME_SIZE as isize - 1);
                        self.frames_left -= 1;
                    }
                    if self.frames_left == 0 {
                        continue;
                    }
                    self.swap = false;
                    self.pos = payload + bits as usize / 8;
                }
            }
            return Ok(());
        }
    }

    fn read_payload(&mut self, src: &DssBytes, buf: &mut [u8]) -> std::result::Result<(), Short> {
        let mut done = 0;
        while done < buf.len() {
            let off = (self.pos - self.header_size) % DSS_BLOCK_SIZE;
            if off == 0 {
                let header = src.get(self.pos, DSS_BLOCK_HEADER_SIZE)?;
                if self.other_format(header) {
                    let raw = header[4];
                    self.block_pos = self.pos - DSS_BLOCK_SIZE;
                    return Err(Short::PartEnd(raw));
                }
            }
            if off < DSS_BLOCK_HEADER_SIZE {
                self.pos += DSS_BLOCK_HEADER_SIZE - off;
                continue;
            }
            let chunk = (buf.len() - done).min(DSS_BLOCK_SIZE - off);
            let bytes = src.get(self.pos, chunk)?;
            buf[done..done + chunk].copy_from_slice(bytes);
            self.pos += chunk;
            done += chunk;
        }
        Ok(())
    }

    fn byte_swap(&mut self, pkt: &mut [u8; DSS_SP_PACKET_BUF], size: usize) {
        if self.swap {
            for i in (0..size - 1).step_by(2) {
                pkt[i] = pkt[i + 4];
            }
            pkt[1] = self.swap_byte.unwrap_or(0xff);
        } else {
            self.swap_byte = Some(pkt[size - 1]);
        }
        pkt[size + 1..DSS_SP_FRAME_SIZE + 1].fill(0);
        pkt[size - 1] = 0;
        self.swap = !self.swap;
    }

    fn read_qp_frame(&mut self, src: &DssBytes) -> std::result::Result<DssStep, Short> {
        while self.frames_left == 0 || self.qp_left == 0 {
            match self.qp_next_block(src, 0) {
                Err(Short::End) => return Ok(DssStep::End { cut: self.cut }),
                other => other?,
            };
        }

        let mut frame = [0u8; DSS_QP_FRAME_SIZE];
        let mut offset = 0;
        let mut frame_format = self.qp_block_format;
        self.qp_frame_size = DSS_QP_FRAME_SIZE;
        while offset < self.qp_frame_size {
            let mut size = (self.qp_frame_size - offset).min(self.qp_left);
            if size == 0 {
                match self.qp_next_block(src, offset) {
                    Err(Short::End) => return Ok(self.qp_truncated(frame, offset, frame_format)),
                    Ok(true) => {
                        offset = 0;
                        self.qp_frame_size = DSS_QP_FRAME_SIZE;
                    }
                    other => {
                        other?;
                    }
                }
                continue;
            }

            if offset == 0 {
                frame_format = self.qp_block_format;
                if frame_format == 7 {
                    size = size.min(2);
                }
            }
            let bytes = src.avail(self.pos, size)?;
            let got = bytes.len();
            frame[offset..offset + got].copy_from_slice(bytes);
            self.pos += got;
            if got < size {
                return Ok(self.qp_truncated(frame, offset + got, frame_format));
            }
            if offset == 0 {
                self.frames_left -= 1;
                if frame_format == 7 && frame[1] & 0x80 == 0 {
                    self.qp_frame_size = DSS_QP7_NOISE_FRAME_SIZE;
                }
            }
            offset += size;
            self.qp_left -= size;
        }
        Ok(DssStep::Frame(qp_tag(frame_format), frame[..self.qp_frame_size].to_vec()))
    }

    fn qp_truncated(&mut self, mut frame: [u8; DSS_QP_FRAME_SIZE], offset: usize, format: u8) -> DssStep {
        if offset == 0 {
            self.cut |= self.frames_left > 0;
            return DssStep::End { cut: self.cut };
        }
        frame[offset..].fill(0);
        self.qp_left = 0;
        self.cut = true;
        DssStep::Frame(qp_tag(format), frame[..self.qp_frame_size].to_vec())
    }

    fn qp_next_block(&mut self, src: &DssBytes, in_frame: usize) -> std::result::Result<bool, Short> {
        self.block_pos += DSS_BLOCK_SIZE;
        let header = src.get(self.block_pos, DSS_BLOCK_HEADER_SIZE)?;
        if self.other_format(header) {
            let raw = header[4];
            self.block_pos -= DSS_BLOCK_SIZE;
            return Err(Short::PartEnd(raw));
        }

        self.frames_left = block_frames(header);
        if self.frames_left > 0 && part_format(header_format(header)) == Some(6) {
            self.qp_block_format = header[4];
        }
        let anchor = block_anchor(header);
        self.pos = self.block_pos + DSS_BLOCK_HEADER_SIZE;
        if self.frames_left == 0 {
            self.qp_left = anchor;
            return Ok(false);
        }
        let resync = if in_frame > 0 {
            in_frame + anchor != self.qp_frame_size
        } else {
            anchor != 0
        };
        if resync {
            self.qp_left = DSS_BLOCK_PAYLOAD_SIZE - anchor;
            self.pos += anchor;
            return Ok(true);
        }
        self.qp_left = DSS_BLOCK_PAYLOAD_SIZE;
        Ok(false)
    }
}

fn qp_tag(format: u8) -> u8 {
    if format == 7 {
        7
    } else {
        6
    }
}

pub(crate) struct DssSpStreamDemuxer {
    walker: DssSpWalker,
    buf: Vec<u8>,
    base: usize,
}

impl DssSpStreamDemuxer {
    #[cfg(test)]
    pub(crate) fn new(version: u8, variable_rate: bool) -> Self {
        Self::with_header_size(usize::from(version.max(1)) * DSS_BLOCK_SIZE, variable_rate)
    }

    pub(crate) fn with_header_size(header_size: usize, variable_rate: bool) -> Self {
        Self::with_format(header_size, u8::from(variable_rate))
    }

    pub(crate) fn with_format(header_size: usize, format: u8) -> Self {
        Self {
            walker: DssSpWalker::new(header_size, format),
            buf: Vec::new(),
            base: 0,
        }
    }

    pub(crate) fn from_block(header_size: usize, start: usize, format: u8) -> Self {
        let mut demuxer = Self::with_format(header_size, format);
        demuxer.walker.block_pos = start - DSS_BLOCK_SIZE;
        demuxer.walker.pos = start;
        demuxer
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<TaggedFrames> {
        self.buf.extend_from_slice(data);
        Ok(self.walk(false)?.0)
    }

    pub(crate) fn finish(&mut self) -> Result<TaggedFrames> {
        self.check_header()?;
        let (frames, cut) = self.walk(true)?;
        self.check_codec()?;
        let tail = (self.base + self.buf.len()).saturating_sub(self.walker.header_size);
        if cut && !tail.is_multiple_of(DSS_BLOCK_SIZE) {
            let frame = if self.walker.format == 6 {
                "DS2 QP frame"
            } else {
                "DSS SP frame"
            };
            return Err(DecodeError::Truncated(frame.to_string()));
        }
        Ok(frames)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<TaggedFrames> {
        self.check_header()?;
        let frames = self.walk(true)?.0;
        self.check_codec()?;
        Ok(frames)
    }

    fn check_codec(&self) -> Result<()> {
        match self.walker.unsupported {
            Some(raw) => Err(DecodeError::UnsupportedCodec(raw)),
            None => Ok(()),
        }
    }

    fn check_header(&self) -> Result<()> {
        let received = self.base + self.buf.len();
        if received > 0 && received < self.walker.header_size {
            return Err(DecodeError::Truncated("DSS header".to_string()));
        }
        Ok(())
    }

    fn walk(&mut self, eof: bool) -> Result<(TaggedFrames, bool)> {
        let mut frames = Vec::new();
        let cut = loop {
            let src = DssBytes {
                data: &self.buf,
                base: self.base,
                eof,
            };
            match self.walker.step(&src) {
                DssStep::Frame(tag, frame) => frames.push((tag, frame)),
                DssStep::Dropped => {}
                DssStep::NeedMore => break false,
                DssStep::End { cut } => break cut,
                DssStep::Unsupported(raw) if frames.is_empty() => {
                    return Err(DecodeError::UnsupportedCodec(raw));
                }
                DssStep::Unsupported(_) => break false,
            }
        };

        let unneeded = self
            .walker
            .block_pos
            .saturating_sub(self.base)
            .min(self.buf.len());
        self.buf.drain(..unneeded);
        self.base += unneeded;

        Ok((frames, cut))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(tagged: TaggedFrames) -> Vec<Vec<u8>> {
        tagged.into_iter().map(|(_, frame)| frame).collect()
    }

    fn make_dss_block(
        swap: u8,
        byte1: u8,
        frame_count: u8,
        payload_pattern: u8,
    ) -> [u8; DSS_BLOCK_SIZE] {
        let mut block = [0u8; DSS_BLOCK_SIZE];
        block[0] = swap << 7;
        block[1] = byte1;
        block[2] = frame_count;
        block[3] = 0xff;
        for (i, byte) in block[DSS_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }
        block
    }

    fn make_dss_file(blocks: &[[u8; DSS_BLOCK_SIZE]]) -> Vec<u8> {
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        for block in blocks {
            data.extend_from_slice(block);
        }
        data
    }

    fn pause_block(byte1: u8, byte5: u8, payload_pattern: u8) -> [u8; DSS_BLOCK_SIZE] {
        let mut block = make_dss_block(0, byte1, 0, payload_pattern);
        block[5] = byte5;
        block
    }

    fn collect_frames(
        demuxer: &mut DssSpStreamDemuxer,
        data: &[u8],
        chunk_size: usize,
    ) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for chunk in data.chunks(chunk_size) {
            frames.extend(frames_of(demuxer.push(chunk).unwrap()));
        }
        frames.extend(frames_of(demuxer.finish().unwrap()));
        frames
    }

    #[test]
    fn test_dss_stream_demux_matches_batch() {
        let data = make_dss_file(&[make_dss_block(0, 0, 3, 0x20)]);

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let actual = collect_frames(&mut demuxer, &data, 149);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_stream_demux_empty_block_reset_matches_batch() {
        let data = make_dss_file(&[
            make_dss_block(0, 0, 13, 0x10),
            make_dss_block(1, 17, 0, 0x80),
            make_dss_block(1, 0, 1, 0xC0),
        ]);

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let actual = collect_frames(&mut demuxer, &data, 127);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_demux_frame_across_block_header() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 14);
        assert_eq!(frames.len(), 14);
        let last_whole = &frames[12];
        assert_eq!(last_whole[0], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 492]);
        assert_eq!(last_whole[13], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 505]);
        assert_eq!(last_whole[14], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE]);
        assert_eq!(frames[13][1], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 26]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 61), frames);
    }

    #[test]
    fn test_dss_demux_reads_swap_byte_from_file() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 1, 0x10),
            make_dss_block(1, 20, 1, 0x80),
        ]);

        let (frames, _) = demux_dss(&data).unwrap();
        assert_eq!(frames.len(), 2);
        let carried = data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 40];
        let in_file = data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 34];
        assert_ne!(carried, in_file);
        assert_eq!(frames[1][1], in_file);
        assert_eq!(frames[1][40], 0);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 7), frames);
    }

    #[test]
    fn test_dss_demux_drops_swapped_frame_without_swap_byte() {
        let data = make_dss_file(&[make_dss_block(1, 2, 3, 0x40)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames.len(), 2);
        let payload = &data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..];
        assert_eq!(frames[0][..40], payload[40..80]);
        assert_eq!(frames[1][1], payload[80]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 33), frames);
    }

    #[test]
    fn test_dss_swapped_frame_starts_before_the_payload() {
        let first = make_dss_block(1, 3, 13, 0x40);
        let second = make_dss_block(0, 17, 5, 0x90);
        let (expected, total) = demux_dss(&make_dss_file(&[first, second])).unwrap();
        assert_eq!((expected.len(), total), (18, 18));
        let mut stream = first[DSS_BLOCK_HEADER_SIZE..].to_vec();
        stream.extend_from_slice(&second[DSS_BLOCK_HEADER_SIZE..]);
        let payload = DSS_BLOCK_SIZE - DSS_BLOCK_HEADER_SIZE;
        for moved in [2usize, 4, 6] {
            let (mut a, mut b) = (first, second);
            a[1] -= moved as u8 / 2;
            b[1] -= moved as u8 / 2;
            a[DSS_BLOCK_HEADER_SIZE..].copy_from_slice(&stream[moved..moved + payload]);
            b[DSS_BLOCK_HEADER_SIZE..].fill(0);
            b[DSS_BLOCK_HEADER_SIZE..DSS_BLOCK_SIZE - moved]
                .copy_from_slice(&stream[moved + payload..]);
            let mut empty = make_dss_block(0, 3, 0, 0);
            empty[DSS_BLOCK_HEADER_SIZE..].fill(0);
            empty[DSS_BLOCK_SIZE - moved..].copy_from_slice(&stream[..moved]);

            let data = make_dss_file(&[empty, a, b]);
            assert_eq!(
                demux_dss(&data).unwrap(),
                (expected.clone(), 18),
                "{moved} bytes"
            );
            for chunk in [1, 7] {
                let mut demuxer = DssSpStreamDemuxer::new(2, false);
                assert_eq!(collect_frames(&mut demuxer, &data, chunk), expected);
            }

            let alone = make_dss_file(&[a, b]);
            assert_eq!(
                demux_dss(&alone).unwrap(),
                (expected[1..].to_vec(), 17),
                "{moved} bytes, first block"
            );
            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            assert_eq!(collect_frames(&mut demuxer, &alone, 7), expected[1..]);
        }
    }

    #[test]
    fn test_dss_demux_partial_last_block() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 124;

        let (frames, _) = demux_dss(&whole[..end]).unwrap();
        let (expected, _) = demux_dss(&whole).unwrap();
        assert_eq!(frames, expected);
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &whole[..end], 50), frames);
    }

    #[test]
    fn test_dss_demux_cut_block_header_ends_cleanly() {
        let mut data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        data.extend_from_slice(&[0x00, 0x03, 0x05]);

        let (frames, _) = demux_dss(&data).unwrap();
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 100).len(), 3);
    }

    #[test]
    fn test_dss_demux_cut_frame_is_dropped() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 123;

        let (frames, total) = demux_dss(&whole[..end]).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames.len(), 2);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&whole[..end]).unwrap();
        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = frames_of(demuxer.push(&whole[..end]).unwrap());
        lenient.extend(frames_of(demuxer.finish_lenient().unwrap()));
        assert_eq!(lenient, frames);
    }

    #[test]
    fn test_dss_demux_overdeclared_frames_stop_at_end_of_file() {
        let data = make_dss_file(&[make_dss_block(0, 3, 13, 0x20)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 12);
        assert_eq!(frames.len(), 12);
        assert!(frames.iter().all(|f| f.len() == DSS_SP_FRAME_SIZE));
    }

    #[test]
    fn test_dss_demux_pause_block_frames_from_byte5() {
        let data = make_dss_file(&[pause_block(3, 3, 0x20)]);
        let mut counted = data.clone();
        counted[2 * DSS_BLOCK_SIZE + 2] = 3;

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 3);
        assert_eq!(frames[0][..40], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
        assert_eq!(frames, demux_dss(&counted).unwrap().0);
    }

    #[test]
    fn test_dss_stream_demux_pause_block_matches_batch() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 11, 0x10),
            pause_block(3, 2, 0x50),
            make_dss_block(0, 3, 1, 0x90),
        ]);

        let (expected, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 14);
        assert_eq!(expected[11][..40], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 37), expected);
    }

    #[test]
    fn test_dss_demux_byte5_ff_leaves_block_empty() {
        let data = make_dss_file(&[pause_block(3, 0xff, 0x20), make_dss_block(0, 3, 2, 0x60)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames[0][..40], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
    }

    #[test]
    fn test_block_frames_reads_byte5_when_byte2_is_0_or_ff() {
        for (header, frames) in [
            ([0x0f, 0x03, 0x0a, 0xff, 0x06, 0x07], 10),
            ([0x01, 0x00, 0x00, 0xc0, 0x06, 0x09], 9),
            ([0x0f, 0x03, 0xff, 0xff, 0x06, 0x09], 9),
            ([0x0f, 0x03, 0xff, 0xff, 0x06, 0xff], 0),
            ([0x0f, 0x03, 0x00, 0xff, 0x06, 0xff], 0),
            ([0x0f, 0x03, 0x00, 0xff, 0x06, 0x00], 0),
        ] {
            assert_eq!(block_frames(&header), frames, "{header:02x?}");
        }
    }

    #[test]
    fn test_block_frames_over_13_empties_sp_and_qp_blocks() {
        for (header, frames) in [
            ([0x0f, 0x04, 0x13, 0xff, 0x06, 0xff], 0),
            ([0x0f, 0x03, 0x00, 0xff, 0x06, 0x13], 0),
            ([0x0f, 0x03, 0x0e, 0xff, 0x00, 0xff], 0),
            ([0x0f, 0x03, 0x0e, 0xff, 0x05, 0xff], 0),
            ([0x0f, 0x03, 0x0d, 0xff, 0x06, 0xff], 13),
            ([0x0f, 0x03, 0x0d, 0xff, 0x00, 0xff], 13),
            ([0x0f, 0x0c, 0x1d, 0xff, 0x01, 0xff], 29),
            ([0x0f, 0x03, 0x16, 0xff, 0x02, 0xff], 22),
            ([0x0f, 0x03, 0x16, 0xff, 0x04, 0xff], 22),
            ([0x0f, 0x04, 0x2a, 0xff, 0x07, 0xff], 42),
            ([0x0f, 0x0c, 0x1d, 0x00, 0x01, 0xff], 0),
            ([0x0f, 0x03, 0x16, 0x00, 0x02, 0xff], 0),
            ([0x0f, 0x03, 0x16, 0x00, 0x04, 0xff], 0),
            ([0x0f, 0x04, 0x2a, 0x00, 0x07, 0xff], 0),
            ([0x01, 0x00, 0x00, 0xc0, 0x07, 0x13], 0),
            ([0x0f, 0x04, 0x0d, 0x00, 0x06, 0xff], 13),
        ] {
            assert_eq!(block_frames(&header), frames, "{header:02x?}");
        }
    }

    #[test]
    fn test_dss_demux_pause_block_frames_at_its_pointer() {
        for (byte3, frames, swap, byte1) in [(0x70u8, 6u8, 0u8, 23u8), (0x78, 11, 1, 23)] {
            let mut pause = make_dss_block(0, 0, 0, 0x30);
            pause[..6].copy_from_slice(&[0x01, 0x00, 0x00, byte3, 0x00, frames]);
            let data = make_dss_file(&[pause]);
            let olympus = make_dss_file(&[make_dss_block(swap, byte1, frames, 0x30)]);

            let (expected, total) = demux_dss(&olympus).unwrap();
            assert_eq!(total, usize::from(frames));
            assert_eq!(demux_dss(&data).unwrap(), (expected.clone(), total));
            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            assert_eq!(collect_frames(&mut demuxer, &data, 37), expected);
        }
    }

    #[test]
    fn test_dss_demux_byte2_overrides_byte5() {
        let mut block = make_dss_block(0, 3, 1, 0x20);
        block[5] = 7;
        let data = make_dss_file(&[block]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 1);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn test_dss_demux_pause_block_count_past_end_of_file() {
        let data = make_dss_file(&[pause_block(3, 13, 0x20)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 12);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 37), frames);

        let end = data.len() - 100;
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..end]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = frames_of(demuxer.push(&data[..end]).unwrap());
        lenient.extend(frames_of(demuxer.finish_lenient().unwrap()));
        assert_eq!(lenient, demux_dss(&data[..end]).unwrap().0);
    }

    #[test]
    fn test_dss_stream_demux_truncated_frame() {
        let data = make_dss_file(&[make_dss_block(0, 0, 13, 0x30)]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        for chunk in data[..data.len() - 100].chunks(211) {
            let _ = demuxer.push(chunk).unwrap();
        }

        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_dss_stream_demux_frame_past_block_boundary_is_complete() {
        let whole = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);
        let data = &whole[..3 * DSS_BLOCK_SIZE];

        let (frames, _) = demux_dss(data).unwrap();
        assert_eq!(frames.len(), 12);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, data, 70), frames);
    }

    #[test]
    fn test_dss_stream_demux_frame_into_cut_block_header_fails() {
        let whole = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);
        for cut in [1, 5, DSS_BLOCK_HEADER_SIZE] {
            let data = &whole[..3 * DSS_BLOCK_SIZE + cut];
            let (frames, _) = demux_dss(data).unwrap();
            assert_eq!(frames.len(), 12);

            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            let _ = demuxer.push(data).unwrap();
            assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            let mut lenient = frames_of(demuxer.push(data).unwrap());
            lenient.extend(frames_of(demuxer.finish_lenient().unwrap()));
            assert_eq!(lenient, frames);
        }
    }

    #[test]
    fn test_dss_stream_demux_cut_before_stated_frame_fails() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 5, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 124;
        let (frames, _) = demux_dss(&whole[..end]).unwrap();
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&whole[..end]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = frames_of(demuxer.push(&whole[..end]).unwrap());
        lenient.extend(frames_of(demuxer.finish_lenient().unwrap()));
        assert_eq!(lenient, frames);
    }

    #[test]
    fn test_dss_stream_demux_truncated_header() {
        let data = make_dss_file(&[]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..100]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..100]).unwrap();
        assert!(matches!(demuxer.finish_lenient(), Err(DecodeError::Truncated(_))));
    }

    #[test]
    fn test_dss_demux_truncated_header() {
        let data = make_dss_file(&[]);
        for cut in [4, 100, data.len() - 1] {
            assert!(matches!(
                demux_dss(&data[..cut]),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(&data[..cut]),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
        }
        let (frames, total) = demux_dss(&data).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_dss_demux_refuses_lp_and_unknown_codecs() {
        for codec in [2u8, 3, 0xff] {
            let mut block = make_dss_block(0, 3, 3, 0x20);
            block[4] = codec;
            assert!(matches!(
                demux_dss(&make_dss_file(&[block])),
                Err(DecodeError::UnsupportedCodec(c)) if c == codec
            ));
        }
        let data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        assert_eq!(demux_dss(&data).unwrap().1, 3);
    }

    #[test]
    fn test_dss_demux_ignores_header_codec_byte() {
        let data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let (expected, _) = demux_dss(&data).unwrap();
        for byte in [1u8, 2, 0xff] {
            let mut marked = data.clone();
            marked[0x2a4] = byte;
            let (frames, total) = demux_dss(&marked).unwrap();
            assert_eq!(total, 3);
            assert_eq!(frames, expected);
        }
    }

    #[test]
    fn test_dss_demux_takes_codec_from_first_block_with_frames() {
        let mut empty = make_dss_block(0, 3, 0, 0x10);
        empty[4] = 2;
        let data = make_dss_file(&[empty, make_dss_block(0, 3, 3, 0x20)]);
        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 3);
        let (expected, _) = demux_dss(&make_dss_file(&[make_dss_block(0, 3, 3, 0x20)])).unwrap();
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_dss_demux_refuses_lp_after_empty_block() {
        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[make_dss_block(0, 3, 0, 0x10), lp]);
        assert!(matches!(demux_dss(&data), Err(DecodeError::UnsupportedCodec(2))));
    }

    #[test]
    fn test_dss_demux_refuses_lp_in_partial_first_block() {
        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[lp]);
        for end in [2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE, 2 * DSS_BLOCK_SIZE + 100] {
            assert!(matches!(
                demux_dss(&data[..end]),
                Err(DecodeError::UnsupportedCodec(2))
            ));
        }
    }

    #[test]
    fn test_dss_demux_without_frames_is_empty() {
        let mut empty = make_dss_block(0, 3, 0, 0x10);
        empty[4] = 2;
        let (frames, total) = demux_dss(&make_dss_file(&[empty, empty])).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);

        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[lp]);
        let (frames, total) = demux_dss(&data[..2 * DSS_BLOCK_SIZE + 5]).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_dss_demux_rejects_other_magic() {
        let mut data = make_dss_file(&[make_dss_block(0, 3, 1, 0x20)]);
        data[3] = b'2';
        assert!(matches!(demux_dss(&data), Err(DecodeError::NotDss(_))));
        assert!(matches!(demux_dss(b"\x02ds"), Err(DecodeError::NotDss(_))));
    }

    fn with_header_blocks(first_byte: u8, audio: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; usize::from(first_byte.max(1)) * DSS_BLOCK_SIZE];
        data[0] = first_byte;
        data[1..4].copy_from_slice(b"dss");
        data.extend_from_slice(audio);
        data
    }

    const EMPTY_HEADER_BLOCK: [u8; DSS_BLOCK_HEADER_SIZE] = [0xc0, 0x5d, 0, 0, 0xff, 0xff];

    #[test]
    fn test_dss_demux_reads_header_of_any_block_count() {
        let base = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);
        let (expected, total) = demux_dss(&base).unwrap();
        for first_byte in [3u8, 6, 8, 19, 255] {
            let data = with_header_blocks(first_byte, &base[2 * DSS_BLOCK_SIZE..]);
            assert_eq!(demux_dss(&data).unwrap(), (expected.clone(), total));
            let mut demuxer = DssSpStreamDemuxer::new(first_byte, false);
            assert_eq!(collect_frames(&mut demuxer, &data, 97), expected);
        }
    }

    #[test]
    fn test_dss_demux_reads_one_block_header() {
        let first = make_dss_block(0, 3, 3, 0x20);
        let second = make_dss_block(0, 3, 3, 0x40);
        let base = make_dss_file(&[first, second]);
        let (expected, _) = demux_dss(&base).unwrap();
        let (copied, _) = demux_dss(&make_dss_file(&[first, first, second])).unwrap();
        for first_byte in [0u8, 1] {
            let mut data = base.clone();
            data[0] = first_byte;
            data[DSS_BLOCK_SIZE..2 * DSS_BLOCK_SIZE].fill(0x55);
            data[DSS_BLOCK_SIZE..DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE]
                .copy_from_slice(&EMPTY_HEADER_BLOCK);
            assert_eq!(demux_dss(&data).unwrap().0, expected);

            data[DSS_BLOCK_SIZE..2 * DSS_BLOCK_SIZE].copy_from_slice(&first);
            assert_eq!(demux_dss(&data).unwrap().0, copied);
            let mut demuxer = DssSpStreamDemuxer::new(first_byte, false);
            assert_eq!(collect_frames(&mut demuxer, &data, 61), copied);
        }
    }

    #[test]
    fn test_dss_demux_long_header_cut_is_truncated() {
        for first_byte in [8u8, 19, 255] {
            let mut data = with_header_blocks(first_byte, &[]);
            data[2 * DSS_BLOCK_SIZE..3 * DSS_BLOCK_SIZE]
                .copy_from_slice(&make_dss_block(0, 3, 3, 0x20));
            data.pop();
            assert!(matches!(
                demux_dss(&data),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
            for strict in [true, false] {
                let mut demuxer = DssSpStreamDemuxer::new(first_byte, false);
                assert!(demuxer.push(&data).unwrap().is_empty());
                let result = if strict {
                    demuxer.finish()
                } else {
                    demuxer.finish_lenient()
                };
                assert!(matches!(
                    result,
                    Err(DecodeError::Truncated(what)) if what == "DSS header"
                ));
            }
        }
    }

    #[test]
    fn test_dss_demux_refuses_lp_after_short_or_long_header() {
        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        for first_byte in [0u8, 1] {
            let mut data = make_dss_file(&[lp]);
            data[0] = first_byte;
            data[DSS_BLOCK_SIZE..DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE]
                .copy_from_slice(&EMPTY_HEADER_BLOCK);
            let result = demux_dss(&data);
            assert!(matches!(result, Err(DecodeError::UnsupportedCodec(2))));
        }
        for first_byte in [8u8, 19] {
            let data = with_header_blocks(first_byte, &lp);
            let result = demux_dss(&data);
            assert!(matches!(result, Err(DecodeError::UnsupportedCodec(2))));
        }
    }

    const PAYLOAD: usize = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE;

    fn vr_payload() -> Vec<u8> {
        let mut payload: Vec<u8> = (1..=100).collect();
        payload[19] |= 0x80;
        payload[58] |= 0x80;
        payload
    }

    fn vr_file(payload: &[u8], swap: u8, byte1: u8, frame_count: u8, codec: u8) -> Vec<u8> {
        let mut block = [0u8; DSS_BLOCK_SIZE];
        block[0] = swap << 7;
        block[1] = byte1;
        block[2] = frame_count;
        block[3] = 0xff;
        block[4] = codec;
        block[DSS_BLOCK_HEADER_SIZE..DSS_BLOCK_HEADER_SIZE + payload.len()].copy_from_slice(payload);
        make_dss_file(&[block])
    }

    #[test]
    fn test_dss_variable_rate_frame_sizes() {
        let payload = vr_payload();
        let data = vr_file(&payload, 0, 3, 4, 1);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 4);
        assert!(frames.iter().all(|f| f.len() == DSS_SP_FRAME_SIZE));

        assert_eq!(frames[0][..8], payload[..8]);
        assert_eq!(frames[0][8], 0);
        assert_eq!(frames[0][9], payload[9]);
        assert!(frames[0][10..].iter().all(|&b| b == 0));

        assert_eq!(frames[1][0], payload[11]);
        assert_eq!(frames[1][1], payload[8]);
        assert_eq!(frames[1][3], payload[10]);
        assert_eq!(frames[1][8], 0);
        assert!(frames[1][10..].iter().all(|&b| b == 0));

        assert_eq!(frames[2][..40], payload[18..58]);
        assert_eq!(frames[2][40], 0);
        assert_eq!(frames[2][41], payload[59]);

        assert_eq!(frames[3][0], payload[61]);
        assert_eq!(frames[3][1], payload[58]);
        assert_eq!(frames[3][3], payload[60]);

        for chunk in [1, 7, 100] {
            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            assert_eq!(collect_frames(&mut demuxer, &data, chunk), frames);
        }
    }

    #[test]
    fn test_dss_codec_byte_selects_variable_rate_walk() {
        let payload = vr_payload();
        let (fixed, _) = demux_dss(&vr_file(&payload, 0, 3, 4, 0)).unwrap();
        let (variable, _) = demux_dss(&vr_file(&payload, 0, 3, 4, 1)).unwrap();
        assert_eq!(fixed.len(), 4);
        assert_eq!(fixed[0][..40], payload[..40]);
        assert_ne!(fixed, variable);
    }

    #[test]
    fn test_dss_variable_rate_cut_frame_is_dropped() {
        let data = vr_file(&vr_payload(), 0, 3, 4, 1);
        for (cut, whole) in [(1, 0), (5, 0), (15, 1), (19, 2), (50, 2), (90, 3)] {
            let end = PAYLOAD + cut;
            let (frames, total) = demux_dss(&data[..end]).unwrap();
            assert_eq!(total, whole, "cut at {cut}");

            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            let _ = demuxer.push(&data[..end]).unwrap();
            assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            let mut lenient = frames_of(demuxer.push(&data[..end]).unwrap());
            lenient.extend(frames_of(demuxer.finish_lenient().unwrap()));
            assert_eq!(lenient, frames, "cut at {cut}");
        }
    }

    #[test]
    fn test_dss_variable_rate_unknown_swap_byte_reads_long_frame() {
        let payload: Vec<u8> = (1..=60).collect();
        let data = vr_file(&payload, 1, 2, 2, 1);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 1);
        assert_eq!(frames[0][..8], payload[40..48]);
        assert_eq!(frames[0][9], payload[49]);
        assert!(frames[0][10..].iter().all(|&b| b == 0));

        let mut demuxer = DssSpStreamDemuxer::new(2, true);
        assert_eq!(collect_frames(&mut demuxer, &data, 3), frames);
    }
}
