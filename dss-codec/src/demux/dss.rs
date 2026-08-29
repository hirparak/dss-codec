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

    // Le conteneur se decrit lui-meme : chaque bloc donne l'offset de sa
    // premiere trame et la parite d'echange qui va avec. Plutot que de derouler
    // une marche continue qui peut se desynchroniser sans jamais s'en rendre
    // compte, on repart de ce que chaque bloc declare. Une trame qui deborde
    // sur le bloc suivant se lit sans rien de special, puisque les charges
    // utiles sont concatenees ; et si le debordement ne correspond pas a ce que
    // le bloc suivant annonce, la remise en place est automatique.
    let mut stream: Vec<u8> = Vec::new();
    let mut debuts: Vec<usize> = Vec::with_capacity(blocks.len());
    for b in blocks.iter() {
        debuts.push(stream.len());
        stream.extend_from_slice(&b.payload);
    }

    let mut frame_packets = Vec::with_capacity(total_frames);
    let mut swap_byte: u8 = 0;

    for (bi, b) in blocks.iter().enumerate() {
        if b.frame_count == 0 {
            continue;
        }
        let mut spos = debuts[bi] + b.cont_size.min(b.payload.len());
        let mut swap = b.swap;

        for _ in 0..b.frame_count {
            let mut pkt = [0u8; DSS_SP_FRAME_SIZE + 1];
            if swap != 0 {
                let read_size = 40;
                let end = (spos + read_size).min(stream.len());
                let start = spos.min(end);
                let count = end - start;
                pkt[3..3 + count].copy_from_slice(&stream[start..end]);
                spos += read_size;
                for i in (0..DSS_SP_FRAME_SIZE - 2).step_by(2) {
                    pkt[i] = pkt[i + 4];
                }
                pkt[DSS_SP_FRAME_SIZE] = 0;
                pkt[1] = swap_byte;
            } else {
                let end = (spos + DSS_SP_FRAME_SIZE).min(stream.len());
                let start = spos.min(end);
                let count = end - start;
                pkt[..count].copy_from_slice(&stream[start..end]);
                spos += DSS_SP_FRAME_SIZE;
                swap_byte = pkt[DSS_SP_FRAME_SIZE - 2];
            }
            pkt[DSS_SP_FRAME_SIZE - 2] = 0;
            swap ^= 1;
            frame_packets.push(pkt[..DSS_SP_FRAME_SIZE].to_vec());
        }
    }

    Ok((frame_packets, total_frames))
}

pub(crate) struct DssSpStreamDemuxer {
    header_size: usize,
    header_complete: bool,
    block_buf: Vec<u8>,
    stream_buf: Vec<u8>,
    stream_pos: usize,
    swap: usize,
    swap_byte: u8,
    /// Blocs recus dont les trames restent a emettre : (debut de la premiere
    /// trame dans le flux, nombre de trames, parite d'echange declaree).
    en_attente: VecDeque<(usize, usize, usize)>,
    stream_end_pos: usize,
}

impl DssSpStreamDemuxer {
    pub(crate) fn new(version: u8) -> Self {
        Self {
            header_size: version as usize * DSS_BLOCK_SIZE,
            header_complete: false,
            block_buf: Vec::new(),
            stream_buf: Vec::new(),
            stream_pos: 0,
            swap: 0,
            swap_byte: 0,
            en_attente: VecDeque::new(),
            stream_end_pos: 0,
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
        if !self.en_attente.is_empty() {
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

        // Fin de flux : on emet ce qui reste, en tolerant une derniere trame
        // tronquee.
        let mut frames = Vec::new();
        while let Some((debut, fc, sw)) = self.en_attente.pop_front() {
            self.stream_pos = debut.min(self.stream_buf.len() + self.stream_pos);
            self.swap = sw;
            for _ in 0..fc {
                let taille = if self.swap != 0 { 40 } else { DSS_SP_FRAME_SIZE };
                frames.push(self.extract_packet_padded(taille));
            }
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

        // Comme en mode par lot : le bloc dit lui-meme ou commence sa premiere
        // trame et avec quelle parite. On note cela et on emettra ses trames
        // des que les octets seront la -- la derniere peut deborder sur le
        // bloc suivant.
        let debut = self.stream_end_pos;
        self.stream_buf.extend_from_slice(payload);
        self.stream_end_pos += payload.len();
        if frame_count > 0 {
            self.en_attente.push_back((
                debut + cont_size.min(payload.len()),
                frame_count,
                blk_swap,
            ));
        }

        self.emit_available_frames(frames);
    }

    fn emit_available_frames(&mut self, frames: &mut Vec<Vec<u8>>) {
        while let Some(&(debut, fc, sw)) = self.en_attente.front() {
            // De combien d'octets ce bloc a-t-il besoin en tout ?
            let mut besoin = 0usize;
            let mut p = sw;
            for _ in 0..fc {
                besoin += if p != 0 { 40 } else { DSS_SP_FRAME_SIZE };
                p ^= 1;
            }
            if debut + besoin > self.stream_end_pos {
                break;
            }
            self.stream_pos = debut;
            self.swap = sw;
            for _ in 0..fc {
                let taille = if self.swap != 0 { 40 } else { DSS_SP_FRAME_SIZE };
                frames.push(self.extract_packet(taille));
            }
            self.en_attente.pop_front();
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
        for (debut, _, _) in &mut self.en_attente {
            *debut = debut.saturating_sub(consumed);
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
    fn test_dss_demux_overdeclared_frames_no_panic() {
        // A block that declares more frames than its payload holds makes the
        // frame walk run past the end of the assembled stream; the trailing
        // packets must come back zero-padded instead of panicking.
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        data.extend_from_slice(&make_dss_block(0, 3, 14, 0x20));

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 14);
        assert_eq!(frames.len(), 14);
        assert!(frames.iter().all(|f| f.len() == DSS_SP_FRAME_SIZE));
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
