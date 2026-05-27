/// DS2 file demuxer.
///
/// SP mode (0-1): byte-swap demuxing, returns list of 42-byte packets.
/// QP mode (6-7): continuous bitstream, returns raw byte stream + frame count.
use crate::error::{DecodeError, Result};
const DS2_HEADER_SIZE: usize = 0x600;
const DS2_BLOCK_SIZE: usize = 512;
const DS2_BLOCK_HEADER_SIZE: usize = 6;
const DSS_SP_PACKET_SIZE: usize = 42;
const DS2_QP_FRAME_SIZE: usize = 56;

/// Demux a DS2 file.
/// Returns (frame_data, total_frames, is_qp).
/// For SP: frame_data is a Vec<Vec<u8>> of packets.
/// For QP: frame_data is a single Vec<u8> continuous bitstream.
pub fn demux_ds2(data: &[u8]) -> Result<DemuxedDs2> {
    if data.len() < 4 || data[..4] != *b"\x03ds2" {
        return Err(DecodeError::NotDs2(std::path::PathBuf::from("<bytes>")));
    }

    let num_blocks = (data.len() - DS2_HEADER_SIZE) / DS2_BLOCK_SIZE;
    let format_type = data[DS2_HEADER_SIZE + 4];

    let mut total_frames: usize = 0;
    for bi in 0..num_blocks {
        total_frames += data[DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE + 2] as usize;
    }

    if format_type >= 6 {
        // QP mode: continuous bitstream (no byte-swap)
        let mut stream = Vec::new();
        for bi in 0..num_blocks {
            let bstart = DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE;
            stream
                .extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }
        Ok(DemuxedDs2::Qp {
            stream,
            total_frames,
        })
    } else {
        // SP mode: byte-swap demuxing
        let mut stream = Vec::new();
        for bi in 0..num_blocks {
            let bstart = DS2_HEADER_SIZE + bi * DS2_BLOCK_SIZE;
            stream
                .extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let mut swap = ((data[DS2_HEADER_SIZE] >> 7) & 1) as usize;
        let mut swap_byte: u8 = 0;
        let mut pos: usize = 0;
        let mut frame_packets = Vec::with_capacity(total_frames);

        for _fi in 0..total_frames {
            let mut pkt = [0u8; DSS_SP_PACKET_SIZE + 1];
            if swap != 0 {
                let read_size = 40;
                let end = (pos + read_size).min(stream.len());
                let count = end - pos;
                pkt[3..3 + count].copy_from_slice(&stream[pos..end]);
                pos += read_size;
                for i in (0..DSS_SP_PACKET_SIZE - 2).step_by(2) {
                    pkt[i] = pkt[i + 4];
                }
                pkt[DSS_SP_PACKET_SIZE] = 0;
                pkt[1] = swap_byte;
            } else {
                let end = (pos + DSS_SP_PACKET_SIZE).min(stream.len());
                let count = end - pos;
                pkt[..count].copy_from_slice(&stream[pos..end]);
                pos += DSS_SP_PACKET_SIZE;
                swap_byte = pkt[DSS_SP_PACKET_SIZE - 2];
            }
            pkt[DSS_SP_PACKET_SIZE - 2] = 0;
            swap ^= 1;
            frame_packets.push(pkt[..DSS_SP_PACKET_SIZE].to_vec());
        }

        Ok(DemuxedDs2::Sp {
            packets: frame_packets,
            total_frames,
        })
    }
}

pub enum DemuxedDs2 {
    Sp {
        packets: Vec<Vec<u8>>,
        total_frames: usize,
    },
    Qp {
        stream: Vec<u8>,
        total_frames: usize,
    },
}

pub(crate) struct Ds2SpStreamDemuxer {
    header_complete: bool,
    block_buf: Vec<u8>,
    stream_buf: Vec<u8>,
    pending_frames: usize,
    swap: usize,
    swap_byte: u8,
    have_initial_swap: bool,
}

impl Ds2SpStreamDemuxer {
    pub(crate) fn new() -> Self {
        Self {
            header_complete: false,
            block_buf: Vec::new(),
            stream_buf: Vec::new(),
            pending_frames: 0,
            swap: 0,
            swap_byte: 0,
            have_initial_swap: false,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        let mut offset = 0;

        if !self.header_complete {
            let needed = DS2_HEADER_SIZE.saturating_sub(self.block_buf.len());
            let take = needed.min(data.len());
            self.block_buf.extend_from_slice(&data[..take]);
            offset += take;
            if self.block_buf.len() < DS2_HEADER_SIZE {
                return Ok(frames);
            }
            self.header_complete = true;
            self.block_buf.clear();
        }

        self.block_buf.extend_from_slice(&data[offset..]);
        while self.block_buf.len() >= DS2_BLOCK_SIZE {
            let block: Vec<u8> = self.block_buf.drain(..DS2_BLOCK_SIZE).collect();
            self.process_block(&block, &mut frames);
        }

        Ok(frames)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DS2 header".to_string()));
        }
        if !self.block_buf.is_empty() {
            return Err(DecodeError::Truncated("DS2 block".to_string()));
        }
        if self.pending_frames > 0 {
            return Err(DecodeError::Truncated("DS2 SP frame".to_string()));
        }
        Ok(Vec::new())
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DS2 header".to_string()));
        }

        self.block_buf.clear();

        let mut frames = Vec::with_capacity(self.pending_frames);
        while self.pending_frames > 0 {
            let needed = if self.swap != 0 {
                40
            } else {
                DSS_SP_PACKET_SIZE
            };
            frames.push(self.extract_sp_packet_padded(needed));
            self.pending_frames -= 1;
        }

        Ok(frames)
    }

    fn process_block(&mut self, block: &[u8], frames: &mut Vec<Vec<u8>>) {
        if !self.have_initial_swap {
            self.swap = ((block[0] >> 7) & 1) as usize;
            self.have_initial_swap = true;
        }
        self.pending_frames += block[2] as usize;
        self.stream_buf
            .extend_from_slice(&block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_SIZE]);

        while self.pending_frames > 0 {
            let needed = if self.swap != 0 {
                40
            } else {
                DSS_SP_PACKET_SIZE
            };
            if self.stream_buf.len() < needed {
                break;
            }
            frames.push(self.extract_sp_packet(needed));
            self.pending_frames -= 1;
        }
    }

    fn extract_sp_packet(&mut self, read_size: usize) -> Vec<u8> {
        let mut pkt = [0u8; DSS_SP_PACKET_SIZE + 1];
        let chunk: Vec<u8> = self.stream_buf.drain(..read_size).collect();
        self.fill_sp_packet(&mut pkt, &chunk);
        pkt[..DSS_SP_PACKET_SIZE].to_vec()
    }

    fn extract_sp_packet_padded(&mut self, read_size: usize) -> Vec<u8> {
        let take = read_size.min(self.stream_buf.len());
        let chunk: Vec<u8> = self.stream_buf.drain(..take).collect();
        let mut pkt = [0u8; DSS_SP_PACKET_SIZE + 1];
        self.fill_sp_packet(&mut pkt, &chunk);
        pkt[..DSS_SP_PACKET_SIZE].to_vec()
    }

    fn fill_sp_packet(&mut self, pkt: &mut [u8; DSS_SP_PACKET_SIZE + 1], chunk: &[u8]) {
        if self.swap != 0 {
            pkt[3..3 + chunk.len()].copy_from_slice(chunk);
            for i in (0..DSS_SP_PACKET_SIZE - 2).step_by(2) {
                pkt[i] = pkt[i + 4];
            }
            pkt[DSS_SP_PACKET_SIZE] = 0;
            pkt[1] = self.swap_byte;
        } else {
            pkt[..chunk.len()].copy_from_slice(chunk);
            self.swap_byte = pkt[DSS_SP_PACKET_SIZE - 2];
        }
        pkt[DSS_SP_PACKET_SIZE - 2] = 0;
        self.swap ^= 1;
    }
}

pub(crate) struct Ds2QpStreamDemuxer {
    header_complete: bool,
    block_buf: Vec<u8>,
    stream_buf: Vec<u8>,
    pending_frames: usize,
}

impl Ds2QpStreamDemuxer {
    pub(crate) fn new() -> Self {
        Self {
            header_complete: false,
            block_buf: Vec::new(),
            stream_buf: Vec::new(),
            pending_frames: 0,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        let mut offset = 0;

        if !self.header_complete {
            let needed = DS2_HEADER_SIZE.saturating_sub(self.block_buf.len());
            let take = needed.min(data.len());
            self.block_buf.extend_from_slice(&data[..take]);
            offset += take;
            if self.block_buf.len() < DS2_HEADER_SIZE {
                return Ok(frames);
            }
            self.header_complete = true;
            self.block_buf.clear();
        }

        self.block_buf.extend_from_slice(&data[offset..]);
        while self.block_buf.len() >= DS2_BLOCK_SIZE {
            let block: Vec<u8> = self.block_buf.drain(..DS2_BLOCK_SIZE).collect();
            self.process_block(&block, &mut frames);
        }

        Ok(frames)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DS2 header".to_string()));
        }
        if !self.block_buf.is_empty() {
            return Err(DecodeError::Truncated("DS2 block".to_string()));
        }
        if self.pending_frames > 0 {
            return Err(DecodeError::Truncated("DS2 QP frame".to_string()));
        }
        Ok(Vec::new())
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        if !self.header_complete {
            if self.block_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("DS2 header".to_string()));
        }

        self.block_buf.clear();

        let mut frames = Vec::with_capacity(self.pending_frames);
        while self.pending_frames > 0 {
            let take = DS2_QP_FRAME_SIZE.min(self.stream_buf.len());
            let mut frame = vec![0u8; DS2_QP_FRAME_SIZE];
            let chunk: Vec<u8> = self.stream_buf.drain(..take).collect();
            frame[..chunk.len()].copy_from_slice(&chunk);
            frames.push(frame);
            self.pending_frames -= 1;
        }

        Ok(frames)
    }

    fn process_block(&mut self, block: &[u8], frames: &mut Vec<Vec<u8>>) {
        self.pending_frames += block[2] as usize;
        self.stream_buf
            .extend_from_slice(&block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_SIZE]);

        while self.pending_frames > 0 && self.stream_buf.len() >= DS2_QP_FRAME_SIZE {
            let frame: Vec<u8> = self.stream_buf.drain(..DS2_QP_FRAME_SIZE).collect();
            frames.push(frame);
            self.pending_frames -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_file(mode: u8, frame_count: u8, payload_pattern: u8) -> Vec<u8> {
        let mut data = vec![0u8; DS2_HEADER_SIZE];
        data[..4].copy_from_slice(b"\x03ds2");

        let mut block = [0u8; DS2_BLOCK_SIZE];
        block[2] = frame_count;
        block[4] = mode;
        for (i, byte) in block[DS2_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }

        data.extend_from_slice(&block);
        data
    }

    #[test]
    fn test_ds2_sp_stream_demux_matches_batch() {
        let data = make_ds2_file(0, 4, 0x10);
        let expected = match demux_ds2(&data).unwrap() {
            DemuxedDs2::Sp { packets, .. } => packets,
            _ => panic!("expected DS2 SP packets"),
        };

        let mut demuxer = Ds2SpStreamDemuxer::new();
        let mut actual = Vec::new();
        for chunk in data.chunks(137) {
            actual.extend(demuxer.push(chunk).unwrap());
        }
        actual.extend(demuxer.finish().unwrap());

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ds2_qp_stream_demux_matches_batch() {
        let data = make_ds2_file(6, 3, 0x40);
        let expected = match demux_ds2(&data).unwrap() {
            DemuxedDs2::Qp {
                stream,
                total_frames,
            } => stream[..total_frames * DS2_QP_FRAME_SIZE]
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
    fn test_ds2_qp_stream_demux_truncated_frame() {
        let data = make_ds2_file(6, 10, 0x55);
        let mut demuxer = Ds2QpStreamDemuxer::new();
        for chunk in data.chunks(97) {
            let _ = demuxer.push(chunk).unwrap();
        }

        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }
}
