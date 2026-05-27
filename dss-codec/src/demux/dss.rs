/// DSS block-aware demuxer with byte-swap frame extraction.
///
/// Handles empty blocks (frame_count=0) by only including continuation bytes
/// from empty block payloads, and resetting swap state at block group boundaries.
use crate::error::{DecodeError, Result};
use std::collections::VecDeque;

const DSS_BLOCK_SIZE: usize = 512;
const DSS_BLOCK_HEADER_SIZE: usize = 6;
const DSS_SP_FRAME_SIZE: usize = 42;

struct BlockInfo {
    frame_count: usize,
    swap: usize,
    cont_size: usize,
    payload: Vec<u8>,
}

pub fn demux_dss(data: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    if data.len() < 4 || data[1..4] != *b"dss" || (data[0] != 2 && data[0] != 3) {
        return Err(DecodeError::NotDss(std::path::PathBuf::from("<bytes>")));
    }

    let version = data[0] as usize;
    let header_size = version * DSS_BLOCK_SIZE;
    let num_blocks = (data.len() - header_size) / DSS_BLOCK_SIZE;

    let mut blocks = Vec::with_capacity(num_blocks);
    let mut total_frames: usize = 0;

    for bi in 0..num_blocks {
        let bstart = header_size + bi * DSS_BLOCK_SIZE;
        let byte0 = data[bstart];
        let byte1 = data[bstart + 1] as usize;
        let frame_count = data[bstart + 2] as usize;
        let blk_swap = ((byte0 >> 7) & 1) as usize;
        let cont_size = (2 * byte1 + 2 * blk_swap).saturating_sub(DSS_BLOCK_HEADER_SIZE);
        let payload_end = bstart + DSS_BLOCK_SIZE;
        let payload = data[bstart + DSS_BLOCK_HEADER_SIZE..payload_end].to_vec();
        blocks.push(BlockInfo {
            frame_count,
            swap: blk_swap,
            cont_size,
            payload,
        });
        total_frames += frame_count;
    }

    // Build stream: for empty blocks, only include continuation bytes.
    // Track positions where swap state needs resetting.
    let mut stream = Vec::new();
    let mut swap_reset_positions = std::collections::HashMap::new();
    let mut pos: usize = 0;

    for bi in 0..blocks.len() {
        if blocks[bi].frame_count == 0 {
            let cs = blocks[bi].cont_size.min(blocks[bi].payload.len());
            stream.extend_from_slice(&blocks[bi].payload[..cs]);
            pos += cs;
            // Find next non-empty block and record its swap state
            for nbi in (bi + 1)..blocks.len() {
                if blocks[nbi].frame_count > 0 {
                    swap_reset_positions.insert(pos, blocks[nbi].swap);
                    break;
                }
            }
        } else {
            stream.extend_from_slice(&blocks[bi].payload);
            pos += blocks[bi].payload.len();
        }
    }

    // Byte-swap demuxing
    let mut swap = blocks[0].swap;
    let mut swap_byte: u8 = 0;
    let mut spos: usize = 0;
    let mut frame_packets = Vec::with_capacity(total_frames);

    for _fi in 0..total_frames {
        if let Some(&new_swap) = swap_reset_positions.get(&spos) {
            swap = new_swap;
            swap_byte = 0;
        }

        let mut pkt = [0u8; DSS_SP_FRAME_SIZE + 1];
        if swap != 0 {
            let read_size = 40;
            let end = (spos + read_size).min(stream.len());
            let count = end - spos;
            pkt[3..3 + count].copy_from_slice(&stream[spos..end]);
            spos += read_size;
            for i in (0..DSS_SP_FRAME_SIZE - 2).step_by(2) {
                pkt[i] = pkt[i + 4];
            }
            pkt[DSS_SP_FRAME_SIZE] = 0;
            pkt[1] = swap_byte;
        } else {
            let end = (spos + DSS_SP_FRAME_SIZE).min(stream.len());
            let count = end - spos;
            pkt[..count].copy_from_slice(&stream[spos..end]);
            spos += DSS_SP_FRAME_SIZE;
            swap_byte = pkt[DSS_SP_FRAME_SIZE - 2];
        }
        pkt[DSS_SP_FRAME_SIZE - 2] = 0;
        swap ^= 1;
        frame_packets.push(pkt[..DSS_SP_FRAME_SIZE].to_vec());
    }

    Ok((frame_packets, total_frames))
}

pub(crate) struct DssSpStreamDemuxer {
    header_size: usize,
    header_complete: bool,
    block_buf: Vec<u8>,
    stream_buf: Vec<u8>,
    stream_pos: usize,
    pending_frames: usize,
    swap: usize,
    swap_byte: u8,
    have_initial_swap: bool,
    stream_end_pos: usize,
    pending_reset_positions: Vec<usize>,
    scheduled_resets: VecDeque<(usize, usize)>,
}

impl DssSpStreamDemuxer {
    pub(crate) fn new(version: u8) -> Self {
        Self {
            header_size: version as usize * DSS_BLOCK_SIZE,
            header_complete: false,
            block_buf: Vec::new(),
            stream_buf: Vec::new(),
            stream_pos: 0,
            pending_frames: 0,
            swap: 0,
            swap_byte: 0,
            have_initial_swap: false,
            stream_end_pos: 0,
            pending_reset_positions: Vec::new(),
            scheduled_resets: VecDeque::new(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        let mut offset = 0;

        if !self.header_complete {
            let needed = self.header_size.saturating_sub(self.block_buf.len());
            let take = needed.min(data.len());
            self.block_buf.extend_from_slice(&data[..take]);
            offset += take;
            if self.block_buf.len() < self.header_size {
                return Ok(frames);
            }
            self.header_complete = true;
            self.block_buf.clear();
        }

        self.block_buf.extend_from_slice(&data[offset..]);
        while self.block_buf.len() >= DSS_BLOCK_SIZE {
            let block: Vec<u8> = self.block_buf.drain(..DSS_BLOCK_SIZE).collect();
            self.process_block(&block, &mut frames);
        }

        Ok(frames)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DSS header".to_string()));
        }
        if !self.block_buf.is_empty() {
            return Err(DecodeError::Truncated("DSS block".to_string()));
        }
        if self.pending_frames > 0 {
            return Err(DecodeError::Truncated("DSS SP frame".to_string()));
        }
        Ok(Vec::new())
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DSS header".to_string()));
        }

        self.block_buf.clear();

        let mut frames = Vec::with_capacity(self.pending_frames);
        while self.pending_frames > 0 {
            while let Some(&(reset_pos, new_swap)) = self.scheduled_resets.front() {
                if self.stream_pos != reset_pos {
                    break;
                }
                self.swap = new_swap;
                self.swap_byte = 0;
                self.scheduled_resets.pop_front();
            }

            let needed = if self.swap != 0 { 40 } else { DSS_SP_FRAME_SIZE };
            frames.push(self.extract_packet_padded(needed));
            self.pending_frames -= 1;
            self.compact_stream();
        }

        Ok(frames)
    }

    fn process_block(&mut self, block: &[u8], frames: &mut Vec<Vec<u8>>) {
        let byte0 = block[0];
        let byte1 = block[1] as usize;
        let frame_count = block[2] as usize;
        let blk_swap = ((byte0 >> 7) & 1) as usize;
        let cont_size = (2 * byte1 + 2 * blk_swap).saturating_sub(DSS_BLOCK_HEADER_SIZE);
        let payload = &block[DSS_BLOCK_HEADER_SIZE..];

        if !self.have_initial_swap {
            self.swap = blk_swap;
            self.have_initial_swap = true;
        }

        if frame_count == 0 {
            let cs = cont_size.min(payload.len());
            self.stream_buf.extend_from_slice(&payload[..cs]);
            self.stream_end_pos += cs;
            self.pending_reset_positions.push(self.stream_end_pos);
        } else {
            if !self.pending_reset_positions.is_empty() {
                for pos in self.pending_reset_positions.drain(..) {
                    self.scheduled_resets.push_back((pos, blk_swap));
                }
            }
            self.stream_buf.extend_from_slice(payload);
            self.stream_end_pos += payload.len();
            self.pending_frames += frame_count;
        }

        self.emit_available_frames(frames);
    }

    fn emit_available_frames(&mut self, frames: &mut Vec<Vec<u8>>) {
        while self.pending_frames > 0 {
            while let Some(&(reset_pos, new_swap)) = self.scheduled_resets.front() {
                if self.stream_pos != reset_pos {
                    break;
                }
                self.swap = new_swap;
                self.swap_byte = 0;
                self.scheduled_resets.pop_front();
            }

            let needed = if self.swap != 0 {
                40
            } else {
                DSS_SP_FRAME_SIZE
            };
            if self.available_stream() < needed {
                break;
            }

            frames.push(self.extract_packet(needed));
            self.pending_frames -= 1;
            self.compact_stream();
        }
    }

    fn extract_packet(&mut self, read_size: usize) -> Vec<u8> {
        let mut pkt = [0u8; DSS_SP_FRAME_SIZE + 1];
        let end = self.stream_pos + read_size;
        let chunk = self.stream_buf[self.stream_pos..end].to_vec();
        self.fill_packet(&mut pkt, chunk);
        self.stream_pos = end;
        pkt[..DSS_SP_FRAME_SIZE].to_vec()
    }

    fn extract_packet_padded(&mut self, read_size: usize) -> Vec<u8> {
        let take = read_size.min(self.available_stream());
        let end = self.stream_pos + take;
        let chunk = self.stream_buf[self.stream_pos..end].to_vec();
        let mut pkt = [0u8; DSS_SP_FRAME_SIZE + 1];
        self.fill_packet(&mut pkt, chunk);
        self.stream_pos = end;
        pkt[..DSS_SP_FRAME_SIZE].to_vec()
    }

    fn fill_packet(&mut self, pkt: &mut [u8; DSS_SP_FRAME_SIZE + 1], chunk: Vec<u8>) {
        if self.swap != 0 {
            pkt[3..3 + chunk.len()].copy_from_slice(&chunk);
            for i in (0..DSS_SP_FRAME_SIZE - 2).step_by(2) {
                pkt[i] = pkt[i + 4];
            }
            pkt[DSS_SP_FRAME_SIZE] = 0;
            pkt[1] = self.swap_byte;
        } else {
            pkt[..chunk.len()].copy_from_slice(&chunk);
            self.swap_byte = pkt[DSS_SP_FRAME_SIZE - 2];
        }
        pkt[DSS_SP_FRAME_SIZE - 2] = 0;
        self.swap ^= 1;
    }

    fn available_stream(&self) -> usize {
        self.stream_buf.len().saturating_sub(self.stream_pos)
    }

    fn compact_stream(&mut self) {
        if self.stream_pos == 0 {
            return;
        }
        if self.stream_pos >= self.stream_buf.len() {
            self.stream_buf.clear();
            self.stream_pos = 0;
            return;
        }
        self.stream_buf.drain(..self.stream_pos);
        let consumed = self.stream_pos;
        self.stream_pos = 0;
        for pos in &mut self.pending_reset_positions {
            *pos = pos.saturating_sub(consumed);
        }
        for (pos, _) in &mut self.scheduled_resets {
            *pos = pos.saturating_sub(consumed);
        }
        self.stream_end_pos = self.stream_end_pos.saturating_sub(consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for (i, byte) in block[DSS_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }
        block
    }

    fn collect_frames(
        demuxer: &mut DssSpStreamDemuxer,
        data: &[u8],
        chunk_size: usize,
    ) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for chunk in data.chunks(chunk_size) {
            frames.extend(demuxer.push(chunk).unwrap());
        }
        frames.extend(demuxer.finish().unwrap());
        frames
    }

    #[test]
    fn test_dss_stream_demux_matches_batch() {
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        data.extend_from_slice(&make_dss_block(0, 0, 3, 0x20));

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2);
        let actual = collect_frames(&mut demuxer, &data, 149);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_stream_demux_empty_block_reset_matches_batch() {
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        data.extend_from_slice(&make_dss_block(0, 0, 13, 0x10));
        data.extend_from_slice(&make_dss_block(1, 17, 0, 0x80));
        data.extend_from_slice(&make_dss_block(1, 0, 1, 0xC0));

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2);
        let actual = collect_frames(&mut demuxer, &data, 127);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_stream_demux_truncated_frame() {
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        data.extend_from_slice(&make_dss_block(0, 0, 20, 0x30));

        let mut demuxer = DssSpStreamDemuxer::new(2);
        for chunk in data.chunks(211) {
            let _ = demuxer.push(chunk).unwrap();
        }

        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }
}
