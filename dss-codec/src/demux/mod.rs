pub mod dss;
pub mod ds2;
pub mod grundig;

use crate::demux::ds2::{detect_ds2_format_type, DS2_BLOCK_HEADER_SIZE};
use crate::error::{DecodeError, Result};

/// Detected audio format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// Pure DSS file (.dss), SP codec, 12000 Hz output
    DssSp,
    /// DS2 file (.ds2), SP mode (mode byte 0-1): DSS SP blocks, 12000 Hz output
    Ds2Sp,
    /// DS2 file (.ds2), QP mode (mode byte 6-7), 16000 Hz
    Ds2Qp,
    /// DS2 file (.ds2), QP7 mode (mode byte 7), 16000 Hz
    Ds2Qp7,
    /// DSS file (.dss), LP blocks (format byte 2 or 4): G.723.1, 8000 Hz
    DssLp,
    /// DS2 file (.ds2), LP blocks (format byte 2 or 4): G.723.1, 8000 Hz
    Ds2Lp,
    /// Grundig DSS file (first byte 6, magic "dss", codec byte 0), SP codec at
    /// 12000 Hz output
    GrundigSp,
}

impl AudioFormat {
    pub fn native_sample_rate(&self) -> u32 {
        match self {
            AudioFormat::DssSp | AudioFormat::Ds2Sp => 12000,
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => 16000,
            AudioFormat::DssLp | AudioFormat::Ds2Lp => 8000,
            AudioFormat::GrundigSp => 12000,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            AudioFormat::DssSp | AudioFormat::DssLp => "dss",
            AudioFormat::Ds2Sp | AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 | AudioFormat::Ds2Lp => {
                "ds2"
            }
            AudioFormat::GrundigSp => "dss",
        }
    }
}

/// Result of demuxing a file
pub struct DemuxResult {
    pub format: AudioFormat,
    pub frame_data: FrameData,
    pub total_frames: usize,
}

/// Frame data varies by format
pub enum FrameData {
    /// List of fixed-size packets (DSS SP, DS2 SP)
    Packets(Vec<Vec<u8>>),
    /// Continuous bitstream (DS2 QP)
    Stream(Vec<u8>),
}

/// Detect format from file header bytes
pub fn detect_format(data: &[u8]) -> Option<AudioFormat> {
    if data.len() < 4 {
        return None;
    }
    if let Some(header_size) = dss_header_size(data) {
        let start = audio_start(data, header_size);
        return Some(match detect_ds2_format_type(data, start) {
            0 if data[0] == 6 => AudioFormat::GrundigSp,
            2 | 4 => AudioFormat::DssLp,
            _ => AudioFormat::DssSp,
        });
    }
    if data[..4] == *b"\x03enc" && data.len() >= 0x600 + DS2_BLOCK_HEADER_SIZE {
        return Some(ds2_format(detect_ds2_format_type(data, audio_start(data, 0x600))));
    }
    if let Some(header_size) = ds2_header_size(data) {
        let start = audio_start(data, header_size);
        if data.len() < start + DS2_BLOCK_HEADER_SIZE {
            return None;
        }
        return Some(ds2_format(detect_ds2_format_type(data, start)));
    }
    None
}

fn ds2_format(format_type: u8) -> AudioFormat {
    match format_type {
        7 => AudioFormat::Ds2Qp7,
        6 => AudioFormat::Ds2Qp,
        2 | 4 => AudioFormat::Ds2Lp,
        _ => AudioFormat::Ds2Sp,
    }
}

pub(crate) fn ds2_header_size(data: &[u8]) -> Option<usize> {
    if data.len() < 4 || data[1..4] != *b"ds2" || data[0] == 0 {
        return None;
    }
    if data[0] == 1 && data.get(0x203) != Some(&0xff) {
        return Some(3 * 512);
    }
    Some(usize::from(data[0]) * 512)
}

pub(crate) fn dss_header_size(data: &[u8]) -> Option<usize> {
    if data.len() < 4 || data[1..4] != *b"dss" {
        return None;
    }
    Some(usize::from(data[0].max(1)) * 512)
}

pub(crate) fn audio_start(data: &[u8], header_size: usize) -> usize {
    let mut start = header_size;
    while let Some(copy) = data
        .get(start..)
        .and_then(|rest| dss_header_size(rest).or_else(|| ds2_header_size(rest)))
    {
        start += copy;
    }
    start
}

/// Blocks with frames that must agree before a file with no header is taken
/// for DSS or DS2.
const LOST_HEADER_BLOCKS: usize = 16;
/// How far into a file with no header its first frames are looked for.
/// Recorders can leave a long run of empty blocks in front of them.
const LOST_HEADER_SCAN: usize = 4096;

/// A DSS or DS2 file that lost its header starts straight at its audio
/// blocks. With no magic to match, the blocks are the signature: byte 3 of a
/// block header is 0xff, byte 4 gives a known block format and the anchor
/// stays inside the block. Empty blocks, whose first four bytes are zero, are
/// passed over: some recorders write a long run of them in front of the
/// first frames, and a pause can leave a few more between them. Random bytes
/// and other audio containers do not keep that up for sixteen blocks in a row.
///
/// Returns a blank header to put back in front of the blocks, chosen from
/// the first block format: two blocks named "dss" for SP and LP, three named
/// "ds2" for QP and QP7.
pub(crate) fn lost_header(data: &[u8]) -> Option<Vec<u8>> {
    let mut first = None;
    let mut found = 0;
    for block in data.chunks_exact(512).take(LOST_HEADER_SCAN) {
        if block[3] != 0xff {
            if block[..4] != [0, 0, 0, 0] {
                return None;
            }
            continue;
        }
        let anchor = 2 * usize::from(block[1]) + 2 * usize::from(block[0] >> 7);
        crate::demux::dss::part_format(block[4])?;
        if anchor > 512 {
            return None;
        }
        first.get_or_insert(block[4]);
        found += 1;
        if found == LOST_HEADER_BLOCKS {
            break;
        }
    }
    if found < LOST_HEADER_BLOCKS {
        return None;
    }
    let (blocks, name): (usize, &[u8]) = match first? {
        6 | 7 => (3, b"ds2"),
        _ => (2, b"dss"),
    };
    let mut header = vec![0u8; blocks * 512];
    header[0] = blocks as u8;
    header[1..4].copy_from_slice(name);
    Some(header)
}

pub(crate) fn sp_codec(data: &[u8]) -> Option<u8> {
    let header_size = dss_header_size(data).or_else(|| ds2_header_size(data))?;
    Some(detect_ds2_format_type(data, audio_start(data, header_size)))
}

pub(crate) fn check_sp_codec(codec: u8) -> Result<()> {
    match codec {
        0 | 1 => Ok(()),
        other => Err(DecodeError::UnsupportedCodec(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(count: usize, format: u8) -> Vec<u8> {
        let mut data = vec![0u8; count * 512];
        for block in data.chunks_exact_mut(512) {
            block[1] = 3;
            block[2] = 1;
            block[3] = 0xff;
            block[4] = format;
        }
        data
    }

    #[test]
    fn lost_header_names_the_container_from_the_block_format() {
        for (format, prefix) in [(0u8, b"\x02dss"), (1, b"\x02dss"), (6, b"\x03ds2"), (7, b"\x03ds2")] {
            let header = lost_header(&blocks(16, format)).expect("blocks with no header");
            assert_eq!(&header[..4], prefix, "format {format}");
            assert_eq!(header.len(), usize::from(prefix[0]) * 512);
        }
    }

    #[test]
    fn lost_header_wants_sixteen_blocks_with_frames() {
        assert!(lost_header(&blocks(15, 0)).is_none());
        let mut unknown = blocks(24, 0);
        unknown[12 * 512 + 4] = 9;
        assert!(lost_header(&unknown).is_none());
        let mut broken = blocks(24, 6);
        broken[10 * 512 + 3] = 0;
        assert!(lost_header(&broken).is_none());
        assert!(lost_header(&blocks(24, 9)).is_none());
        assert!(lost_header(&vec![0u8; 24 * 512]).is_none());
        assert!(lost_header(&vec![0xffu8; 24 * 512]).is_none());
    }

    #[test]
    fn lost_header_passes_over_empty_blocks() {
        let mut data = vec![0u8; 61 * 512];
        for block in data.chunks_exact_mut(512) {
            block[4] = 6;
        }
        data.extend(blocks(5, 6));
        data.extend(vec![0u8; 2 * 512]);
        data.extend(blocks(11, 6));
        assert_eq!(&lost_header(&data).expect("empty blocks then QP")[..4], b"\x03ds2");
    }

    fn make_ds2_like_file(magic: [u8; 4], mode: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x600 + 0x200];
        data[..4].copy_from_slice(&magic);
        data[0x600 + 2] = 1;
        data[0x600 + 3] = 0xff;
        data[0x600 + 4] = mode;
        data
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_qp() {
        let data = make_ds2_like_file(*b"\x03enc", 6);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_qp7() {
        let data = make_ds2_like_file(*b"\x03enc", 7);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp7));
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_sp() {
        let data = make_ds2_like_file(*b"\x03enc", 0);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Sp));
    }

    fn make_ds2_with_header_blocks(first_byte: u8, header_blocks: usize, mode: u8) -> Vec<u8> {
        let header_size = header_blocks * 512;
        let mut data = vec![0u8; header_size + 512];
        data[0] = first_byte;
        data[1..4].copy_from_slice(b"ds2");
        data[header_size + 2] = 1;
        data[header_size + 3] = 0xff;
        data[header_size + 4] = mode;
        data
    }

    #[test]
    fn ds2_header_size_follows_first_byte() {
        assert_eq!(ds2_header_size(b"\x01ds2"), Some(1536));
        assert_eq!(ds2_header_size(b"\x02ds2"), Some(1024));
        assert_eq!(ds2_header_size(b"\x03ds2"), Some(1536));
        assert_eq!(ds2_header_size(b"\x07ds2"), Some(3584));
        assert_eq!(ds2_header_size(b"\x08ds2"), Some(4096));
        assert_eq!(ds2_header_size(b"\x10ds2"), Some(8192));
        assert_eq!(ds2_header_size(b"\x11ds2"), Some(8704));
        assert_eq!(ds2_header_size(b"\xffds2"), Some(130560));
    }

    #[test]
    fn ds2_header_size_reads_one_block_when_next_block_is_audio() {
        let mut data = make_ds2_with_header_blocks(1, 1, 6);
        data[0x203] = 0;
        assert_eq!(ds2_header_size(&data), Some(1536));
        assert_eq!(detect_format(&data), None);
        data[0x203] = 0xff;
        assert_eq!(ds2_header_size(&data[..0x203]), Some(1536));
        assert_eq!(ds2_header_size(&data), Some(512));
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn ds2_header_size_rejects_other_prefixes() {
        assert_eq!(ds2_header_size(b"\x00ds2"), None);
        assert_eq!(ds2_header_size(b"\x03dss"), None);
        assert_eq!(ds2_header_size(b"\x03enc"), None);
        assert_eq!(ds2_header_size(b"\x03ds"), None);
        assert_eq!(ds2_header_size(b""), None);
    }

    #[test]
    fn dss_header_size_takes_block_count_from_first_byte() {
        for (first_byte, size) in [
            (0u8, 512),
            (1, 512),
            (2, 1024),
            (3, 1536),
            (6, 3072),
            (8, 4096),
            (19, 9728),
            (255, 130560),
        ] {
            let prefix = [first_byte, b'd', b's', b's'];
            assert_eq!(dss_header_size(&prefix), Some(size));
            let format = if first_byte == 6 {
                AudioFormat::GrundigSp
            } else {
                AudioFormat::DssSp
            };
            assert_eq!(detect_format(&prefix), Some(format));
        }
    }

    #[test]
    fn detect_format_keeps_grundig_for_codec_0_after_six_blocks() {
        let mut data = make_ds2_with_header_blocks(6, 6, 0);
        data[1..4].copy_from_slice(b"dss");
        assert_eq!(detect_format(&data), Some(AudioFormat::GrundigSp));
        for codec in [1u8, 2, 4, 6, 0xff] {
            data[6 * 512 + 3] = 0xff;
            data[6 * 512 + 4] = codec;
            let format = if codec == 2 || codec == 4 {
                AudioFormat::DssLp
            } else {
                AudioFormat::DssSp
            };
            assert_eq!(detect_format(&data), Some(format));
            assert_eq!(sp_codec(&data), Some(codec));
            assert_eq!(detect_format(&data[..6 * 512 + 5]), Some(AudioFormat::GrundigSp));
            data[6 * 512 + 2] = 0;
            assert_eq!(detect_format(&data), Some(AudioFormat::GrundigSp));
            data[6 * 512 + 2] = 1;
        }
    }

    #[test]
    fn dss_header_size_rejects_other_prefixes() {
        for prefix in [&b"\x02ds2"[..], b"\x08dsx", b"\x03enc", b"\x02ds", b""] {
            assert_eq!(dss_header_size(prefix), None);
        }
        assert_eq!(detect_format(b"\x08dsx"), None);
    }

    #[test]
    fn detect_format_names_lp_files() {
        for codec in [2u8, 4] {
            let data = make_ds2_with_header_blocks(3, 3, codec);
            assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Lp));
            assert_eq!(detect_format(&make_ds2_like_file(*b"\x03enc", codec)), Some(AudioFormat::Ds2Lp));
            let mut dss = make_ds2_with_header_blocks(2, 2, codec);
            dss[1..4].copy_from_slice(b"dss");
            assert_eq!(detect_format(&dss), Some(AudioFormat::DssLp));
        }
        assert_eq!(AudioFormat::DssLp.native_sample_rate(), 8000);
        assert_eq!(AudioFormat::Ds2Lp.native_sample_rate(), 8000);
        assert_eq!(AudioFormat::DssLp.extension(), "dss");
        assert_eq!(AudioFormat::Ds2Lp.extension(), "ds2");
    }

    #[test]
    fn detect_format_reads_mode_after_two_block_header() {
        let data = make_ds2_with_header_blocks(2, 2, 6);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
        let data = make_ds2_with_header_blocks(2, 2, 7);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp7));
    }

    #[test]
    fn detect_format_reads_mode_after_eight_block_header() {
        let data = make_ds2_with_header_blocks(8, 8, 6);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn detect_format_reads_three_block_header_for_first_byte_one() {
        let data = make_ds2_with_header_blocks(1, 3, 7);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp7));
    }

    #[test]
    fn detect_format_waits_for_first_block_header() {
        let data = make_ds2_with_header_blocks(8, 8, 6);
        assert_eq!(detect_format(&data[..8 * 512 + 4]), None);
        assert_eq!(detect_format(&data[..8 * 512]), None);
        assert_eq!(detect_format(&data[..3 * 512 + 5]), None);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn detect_format_needs_whole_first_block_header() {
        let data = make_ds2_with_header_blocks(3, 3, 6);
        assert_eq!(detect_format(&data[..3 * 512 + 5]), None);
        assert_eq!(detect_format(&data[..3 * 512 + 6]), Some(AudioFormat::Ds2Qp));
        let data = make_ds2_like_file(*b"\x03enc", 6);
        assert_eq!(detect_format(&data[..0x600 + 5]), None);
        assert_eq!(detect_format(&data[..0x600 + 6]), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn sp_codec_reads_first_block_with_frames() {
        let mut dss = make_ds2_with_header_blocks(2, 2, 2);
        dss[1..4].copy_from_slice(b"dss");
        assert_eq!(sp_codec(&dss), Some(2));
        dss[0x2a4] = 1;
        assert_eq!(sp_codec(&dss), Some(2));
        assert_eq!(sp_codec(&dss[..1024 + 5]), Some(0));
        dss[1024 + 2] = 0;
        assert_eq!(sp_codec(&dss), Some(0));
        assert_eq!(sp_codec(&make_ds2_with_header_blocks(3, 3, 1)), Some(1));
        assert_eq!(sp_codec(&make_ds2_with_header_blocks(2, 2, 2)), Some(2));
        assert_eq!(sp_codec(b"RIFF"), None);
    }

    fn stale_header_copy(first_byte: u8, codec: u8) -> Vec<u8> {
        let mut copy = vec![0xffu8; usize::from(first_byte) * 512];
        copy[..6].copy_from_slice(&[first_byte, b'd', b's', b's', codec, 0]);
        copy[512..518].copy_from_slice(&[0, 0, 0, 0, 0xff, 0xff]);
        copy
    }

    #[test]
    fn audio_start_skips_stale_header_copies() {
        let plain = make_ds2_with_header_blocks(3, 3, 6);
        assert_eq!(audio_start(&plain, 0x600), 0x600);
        let mut data = plain.clone();
        data.splice(0x600..0x600, stale_header_copy(3, 1));
        assert_eq!(audio_start(&data, 0x600), 0xc00);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
        assert_eq!(sp_codec(&data), Some(6));
        assert_eq!(audio_start(&data[..0x603], 0x600), 0x600);
        assert_eq!(audio_start(&data[..0x604], 0x600), 0xc00);
        assert_eq!(detect_format(&data[..0xc05]), None);
        assert_eq!(detect_format(&data[..0xc06]), Some(AudioFormat::Ds2Qp));

        let mut dss = make_ds2_with_header_blocks(2, 2, 1);
        dss[1..4].copy_from_slice(b"dss");
        dss.splice(0x400..0x400, stale_header_copy(2, 2));
        assert_eq!(audio_start(&dss, 0x400), 0x800);
        assert_eq!(sp_codec(&dss), Some(1));
        dss.splice(0x400..0x400, stale_header_copy(3, 2));
        assert_eq!(audio_start(&dss, 0x400), 0xe00);
        assert_eq!(sp_codec(&dss), Some(1));
        assert_eq!(detect_format(&dss), Some(AudioFormat::DssSp));
    }

    #[test]
    fn check_sp_codec_accepts_sp_and_variable_rate_only() {
        assert!(check_sp_codec(0).is_ok());
        assert!(check_sp_codec(1).is_ok());
        for codec in [2u8, 3, 5, 6, 0xff] {
            assert!(matches!(
                check_sp_codec(codec),
                Err(DecodeError::UnsupportedCodec(c)) if c == codec
            ));
        }
    }

    #[test]
    fn ds2_sp_rate_matches_dss_sp() {
        assert_eq!(AudioFormat::Ds2Sp.native_sample_rate(), 12000);
        assert_eq!(AudioFormat::DssSp.native_sample_rate(), 12000);
    }

    #[test]
    fn detect_format_rejects_out_of_range_header_byte() {
        let mut data = make_ds2_with_header_blocks(3, 3, 6);
        data[0] = 0;
        assert_eq!(detect_format(&data), None);
    }

    #[test]
    fn detect_format_reads_mode_after_header_over_sixteen_blocks() {
        let data = make_ds2_with_header_blocks(17, 17, 6);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
        let data = make_ds2_with_header_blocks(255, 255, 7);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp7));
    }
}
