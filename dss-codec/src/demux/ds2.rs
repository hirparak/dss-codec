/// DS2 file demuxer.
///
/// SP mode (0-1): byte-swap demuxing, returns list of 42-byte packets.
/// QP mode (6): segmented continuous bitstream.
/// QP7 mode (7): segmented 12/56-byte byte-aligned records.
use crate::error::{DecodeError, Result};

const DS2_HEADER_SIZE: usize = 0x600;
const DS2_BLOCK_SIZE: usize = 512;
const DS2_BLOCK_HEADER_SIZE: usize = 6;
const DSS_SP_PACKET_SIZE: usize = 42;
const DS2_QP_FRAME_SIZE: usize = 56;
const DS2_QP7_PACKET_SHORT_SIZE: usize = 12;
const DS2_QP7_PACKET_LONG_SIZE: usize = 56;

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

pub(crate) fn is_ds2_audio_block_header(block_header: &[u8]) -> bool {
    if block_header.len() < DS2_BLOCK_HEADER_SIZE {
        return false;
    }
    let fc = block_header[2];
    let fmt = block_header[4];
    block_header[0] == 0x0f
        && block_header[3] == 0xff
        && block_header[5] == 0xff
        && fc > 0
        && matches!(fmt, 0 | 1 | 2 | 3 | 6 | 7)
}

pub(crate) fn detect_ds2_audio_start(data: &[u8]) -> usize {
    if data.first().copied() != Some(0x07) {
        return DS2_HEADER_SIZE;
    }

    let scan_end = data
        .len()
        .saturating_sub(DS2_BLOCK_HEADER_SIZE)
        .min(0x10000);
    let mut best_start = None;
    let mut best_score = 0usize;

    let mut offset = DS2_BLOCK_SIZE;
    while offset <= scan_end {
        if !is_ds2_audio_block_header(&data[offset..offset + DS2_BLOCK_HEADER_SIZE]) {
            offset += DS2_BLOCK_SIZE;
            continue;
        }

        let mut valid = 0usize;
        for i in 0..16 {
            let block_start = offset + i * DS2_BLOCK_SIZE;
            if block_start + DS2_BLOCK_HEADER_SIZE > data.len() {
                break;
            }
            if !is_ds2_audio_block_header(&data[block_start..block_start + DS2_BLOCK_HEADER_SIZE]) {
                break;
            }
            valid += 1;
        }

        if valid >= 4 {
            return offset;
        }
        if valid > best_score {
            best_start = Some(offset);
            best_score = valid;
        }

        offset += DS2_BLOCK_SIZE;
    }

    best_start.unwrap_or(0x1000)
}

pub(crate) fn detect_ds2_format_type(data: &[u8], header_size: usize) -> u8 {
    let num_blocks = data.len().saturating_sub(header_size) / DS2_BLOCK_SIZE;
    for bi in 0..num_blocks {
        let bstart = header_size + bi * DS2_BLOCK_SIZE;
        if bstart + DS2_BLOCK_HEADER_SIZE > data.len() {
            break;
        }
        if data[bstart + 2] > 0 {
            return data[bstart + 4];
        }
    }
    0
}

/// Demux a DS2 file.
/// Returns (frame_data, total_frames, is_qp).
/// For SP: frame_data is a Vec<Vec<u8>> of packets.
/// For QP/QP7: frame data is segmented to preserve state resets at cut points.
pub fn demux_ds2(data: &[u8]) -> Result<DemuxedDs2> {
    if data.len() < 4 || !matches!(&data[..4], b"\x03ds2" | b"\x01ds2" | b"\x07ds2") {
        return Err(DecodeError::NotDs2(std::path::PathBuf::from("<bytes>")));
    }

    let header_size = detect_ds2_audio_start(data);
    let num_blocks = data.len().saturating_sub(header_size) / DS2_BLOCK_SIZE;
    let format_type = detect_ds2_format_type(data, header_size);

    let mut total_frames: usize = 0;
    for bi in 0..num_blocks {
        total_frames += data[header_size + bi * DS2_BLOCK_SIZE + 2] as usize;
    }

    if format_type == 7 {
        let payload_size = DS2_BLOCK_SIZE - DS2_BLOCK_HEADER_SIZE;
        let mut raw = Vec::with_capacity(num_blocks * payload_size);
        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            raw.extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let read_record = |raw: &[u8], pos: usize| -> Result<(Vec<u8>, usize)> {
            if pos + 2 > raw.len() {
                return Err(DecodeError::Truncated(format!(
                    "incomplete format-7 selector at byte {}",
                    pos
                )));
            }
            let size = if (raw[pos + 1] & 0x80) == 0 {
                DS2_QP7_PACKET_SHORT_SIZE
            } else {
                DS2_QP7_PACKET_LONG_SIZE
            };
            if pos + size > raw.len() {
                return Err(DecodeError::Truncated(format!(
                    "incomplete format-7 record at byte {}",
                    pos
                )));
            }
            Ok((raw[pos..pos + size].to_vec(), pos + size))
        };

        let mut segments = Vec::new();
        let mut seg_records: Vec<Vec<u8>> = Vec::new();
        let mut raw_read_pos: Option<usize> = None;

        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            let block_header = &data[bstart..bstart + DS2_BLOCK_HEADER_SIZE];
            let fc = block_header[2] as usize;

            if fc == 0 {
                if !seg_records.is_empty() {
                    segments.push(Ds2Qp7Segment {
                        records: std::mem::take(&mut seg_records),
                        total_frames: segments.last().map_or(0, |_| 0),
                        reset_before: !segments.is_empty(),
                    });
                    if let Some(last) = segments.last_mut() {
                        last.total_frames = last.records.len();
                    }
                }
                let zero_end = (bi + 1) * payload_size;
                raw_read_pos = Some(raw_read_pos.unwrap_or(0).max(zero_end));
                continue;
            }

            let payload_off = (block_header[1] as usize * 2).saturating_sub(DS2_BLOCK_HEADER_SIZE);
            let frames_raw_start = bi * payload_size + payload_off;
            raw_read_pos = Some(raw_read_pos.unwrap_or(0).max(frames_raw_start));

            for _ in 0..fc {
                let (record, next_pos) = read_record(&raw, raw_read_pos.unwrap_or(0))?;
                seg_records.push(record);
                raw_read_pos = Some(next_pos);
            }
        }

        if !seg_records.is_empty() {
            segments.push(Ds2Qp7Segment {
                total_frames: seg_records.len(),
                records: seg_records,
                reset_before: !segments.is_empty(),
            });
        }

        Ok(DemuxedDs2::Qp7Segments {
            total_frames: segments.iter().map(|seg| seg.total_frames).sum(),
            segments,
        })
    } else if format_type >= 6 {
        let payload_size = DS2_BLOCK_SIZE - DS2_BLOCK_HEADER_SIZE;
        let mut raw = Vec::with_capacity(num_blocks * payload_size);
        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            raw.extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let mut segments = Vec::new();
        let mut seg_raw_start = 0usize;
        let mut seg_frames = 0usize;
        let mut raw_read_pos = 0usize;
        let mut first_seg = true;

        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            let b1 = data[bstart + 1] as usize;
            let fc = data[bstart + 2] as usize;
            let payload_off = (b1 * 2).saturating_sub(DS2_BLOCK_HEADER_SIZE);
            let frames_raw_start = bi * payload_size + payload_off;

            if bi == 0 {
                raw_read_pos = frames_raw_start;
                seg_raw_start = frames_raw_start;
            } else if frames_raw_start != raw_read_pos {
                let end = raw_read_pos.min(raw.len());
                if seg_frames > 0 && end > seg_raw_start {
                    segments.push(Ds2QpSegment {
                        stream: raw[seg_raw_start..end].to_vec(),
                        total_frames: seg_frames,
                        reset_before: !first_seg,
                    });
                    first_seg = false;
                }
                seg_raw_start = frames_raw_start;
                seg_frames = 0;
                raw_read_pos = frames_raw_start;
            }

            if fc > 0 {
                seg_frames += fc;
                raw_read_pos += fc * DS2_QP_FRAME_SIZE;
            }
        }

        let end = raw_read_pos.min(raw.len());
        if seg_frames > 0 && end > seg_raw_start {
            segments.push(Ds2QpSegment {
                stream: raw[seg_raw_start..end].to_vec(),
                total_frames: seg_frames,
                reset_before: !first_seg,
            });
        }

        Ok(DemuxedDs2::QpSegments {
            total_frames,
            segments,
        })
    } else {
        // SP mode: byte-swap demuxing
        let mut stream = Vec::new();
        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            stream
                .extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let mut swap = ((data[header_size] >> 7) & 1) as usize;
        let mut swap_byte: u8 = 0;
        let mut pos: usize = 0;
        let mut frame_packets = Vec::with_capacity(total_frames);

        for _fi in 0..total_frames {
            // total_frames counts every declared frame, so the walk can run
            // past the end of the stream near EOF; clamp both ends and emit
            // zero-padded packets instead of slicing out of range.
            let mut pkt = [0u8; DSS_SP_PACKET_SIZE + 1];
            if swap != 0 {
                let read_size = 40;
                let end = (pos + read_size).min(stream.len());
                let start = pos.min(end);
                let count = end - start;
                pkt[3..3 + count].copy_from_slice(&stream[start..end]);
                pos += read_size;
                for i in (0..DSS_SP_PACKET_SIZE - 2).step_by(2) {
                    pkt[i] = pkt[i + 4];
                }
                pkt[DSS_SP_PACKET_SIZE] = 0;
                pkt[1] = swap_byte;
            } else {
                let end = (pos + DSS_SP_PACKET_SIZE).min(stream.len());
                let start = pos.min(end);
                let count = end - start;
                pkt[..count].copy_from_slice(&stream[start..end]);
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
    QpSegments {
        segments: Vec<Ds2QpSegment>,
        total_frames: usize,
    },
    Qp7Segments {
        segments: Vec<Ds2Qp7Segment>,
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
    fn test_ds2_sp_demux_overdeclared_frames_no_panic() {
        // A block that declares more frames than its payload holds makes the
        // frame walk run past the end of the assembled stream; the trailing
        // packets must come back zero-padded instead of panicking.
        let data = make_ds2_file(0, 14, 0x20);
        let (packets, total) = match demux_ds2(&data).unwrap() {
            DemuxedDs2::Sp {
                packets,
                total_frames,
            } => (packets, total_frames),
            _ => panic!("expected DS2 SP packets"),
        };
        assert_eq!(total, 14);
        assert_eq!(packets.len(), 14);
        assert!(packets.iter().all(|p| p.len() == DSS_SP_PACKET_SIZE));
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
    fn test_ds2_qp_stream_demux_lenient_batching() {
        let data = make_ds2_file(6, 3, 0x55);
        let mut demuxer = Ds2QpStreamDemuxer::new();
        for chunk in data.chunks(97) {
            let _ = demuxer.push(chunk).unwrap();
        }

        assert_eq!(demuxer.finish().unwrap().len(), 3);
    }
}
