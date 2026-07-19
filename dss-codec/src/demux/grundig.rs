//! Grundig DSS demuxer.
//!
//! Grundig `\x06dss` files store the CELP bitstream in 512-byte blocks. Each
//! block begins with a 6-byte header `[b0][b1][b2] ff 00 ff`; the remaining 506
//! bytes are payload, stored as little-endian 16-bit words whose bits are read
//! MSB-first. `b2` is the number of frames that *start* within the block, and
//! the first frame's bit offset inside the block is `((b1<<8 | b0) >> 4)`
//! decoded as `word_off*16 + (16 - avail)`. Frames are a continuous 328-bit
//! (41-byte) stream; a frame may spill into the next block, but each block's
//! frames always start at that block's own first-frame offset.
//!
//! The whole audio region is buffered and demuxed at `finish`, because frame
//! extraction needs each block's `frame_count` and the cross-block bit reads.

use crate::error::Result;

const BLOCK_SIZE: usize = 512;
const FRAME_BITS: usize = 328;
const BLOCK_BITS: usize = 506 * 8;

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
pub fn demux_grundig(audio: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let nb = audio.len() / BLOCK_SIZE;

    // Per-block payload, frame_count, and first-frame bit offset.
    struct Blk {
        payload: Vec<u8>,
        fc: usize,
        local_first: usize,
    }
    let mut blocks: Vec<Blk> = Vec::with_capacity(nb);
    let mut total_frames = 0usize;
    for bi in 0..nb {
        let blk = &audio[bi * BLOCK_SIZE..bi * BLOCK_SIZE + BLOCK_SIZE];
        let w0 = ((blk[1] as usize) << 8) | (blk[0] as usize);
        let f421 = w0 >> 4;
        let fc = blk[2] as usize;
        let word_off = (f421 >> 4).wrapping_sub(3);
        let avail = 0x10 - (f421 & 0xf);
        let local_first = word_off.wrapping_mul(16) + (16 - avail);
        blocks.push(Blk {
            payload: blk[6..].to_vec(),
            fc,
            local_first,
        });
        total_frames += fc;
    }

    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(total_frames);
    for bi in 0..nb {
        let mut pos = blocks[bi].local_first;
        for _ in 0..blocks[bi].fc {
            let mut need = FRAME_BITS;
            let mut p = pos;
            let mut cb = bi;
            let mut bits: Vec<u8> = Vec::with_capacity(FRAME_BITS);
            while need > 0 {
                let take = need.min(BLOCK_BITS.saturating_sub(p));
                for i in 0..take {
                    bits.push(read_bit(&blocks[cb].payload, p + i));
                }
                need -= take;
                p += take;
                if need > 0 {
                    cb += 1;
                    if cb >= nb {
                        break;
                    }
                    p = 0;
                }
            }
            frames.push(pack_frame(&bits));
            pos = p;
        }
    }

    (frames, total_frames)
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
    /// `header_blocks` = first byte of the file (number of 512-byte header blocks).
    pub(crate) fn new(header_blocks: u8) -> Self {
        Self {
            header_size: header_blocks as usize * BLOCK_SIZE,
            buf: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        self.demux_all()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        self.demux_all()
    }

    fn demux_all(&mut self) -> Result<Vec<Vec<u8>>> {
        if self.buf.len() <= self.header_size {
            return Ok(Vec::new());
        }
        let audio = &self.buf[self.header_size..];
        let (frames, _) = demux_grundig(audio);
        Ok(frames)
    }
}
