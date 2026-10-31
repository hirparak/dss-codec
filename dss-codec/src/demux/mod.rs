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
    /// Grundig DSS file (first byte 6, magic "dss", codec byte 0), SP codec at
    /// 12000 Hz output
    GrundigSp,
}

impl AudioFormat {
    pub fn native_sample_rate(&self) -> u32 {
        match self {
            AudioFormat::DssSp | AudioFormat::Ds2Sp => 12000,
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => 16000,
            AudioFormat::GrundigSp => 12000,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            AudioFormat::DssSp => "dss",
            AudioFormat::Ds2Sp | AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => "ds2",
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
        if data[0] == 6 && detect_ds2_format_type(data, start) == 0 {
            return Some(AudioFormat::GrundigSp);
        }
        return Some(AudioFormat::DssSp);
    }
    if data[..4] == *b"\x03enc" && data.len() >= 0x600 + DS2_BLOCK_HEADER_SIZE {
        let format_type = detect_ds2_format_type(data, audio_start(data, 0x600));
        return Some(match format_type {
            7 => AudioFormat::Ds2Qp7,
            6 => AudioFormat::Ds2Qp,
            _ => AudioFormat::Ds2Sp,
        });
    }
    if let Some(header_size) = ds2_header_size(data) {
        let start = audio_start(data, header_size);
        if data.len() < start + DS2_BLOCK_HEADER_SIZE {
            return None;
        }
        let format_type = detect_ds2_format_type(data, start);
        return Some(match format_type {
            7 => AudioFormat::Ds2Qp7,
            6 => AudioFormat::Ds2Qp,
            _ => AudioFormat::Ds2Sp,
        });
    }
    None
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
        for codec in [1u8, 2, 6, 0xff] {
            data[6 * 512 + 3] = 0xff;
            data[6 * 512 + 4] = codec;
            assert_eq!(detect_format(&data), Some(AudioFormat::DssSp));
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
