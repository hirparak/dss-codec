pub mod dss;
pub mod ds2;
pub mod grundig;

use crate::demux::ds2::{detect_ds2_audio_start, detect_ds2_format_type};

/// Detected audio format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// Pure DSS file (.dss), SP codec at 11025 Hz output
    DssSp,
    /// DS2 file (.ds2), SP mode (mode byte 0-1), 12000 Hz
    Ds2Sp,
    /// DS2 file (.ds2), QP mode (mode byte 6-7), 16000 Hz
    Ds2Qp,
    /// DS2 file (.ds2), QP7 mode (mode byte 7), 16000 Hz
    Ds2Qp7,
    /// Grundig DSS file (first byte 6, magic "dss"), SP codec at 16000 Hz output
    GrundigSp,
}

impl AudioFormat {
    pub fn native_sample_rate(&self) -> u32 {
        match self {
            AudioFormat::DssSp => 11025,
            AudioFormat::Ds2Sp => 12000,
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => 16000,
            AudioFormat::GrundigSp => 16000,
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
    if data[1..4] == *b"dss" && data[0] == 6 {
        return Some(AudioFormat::GrundigSp);
    }
    if data[1..4] == *b"dss" && (data[0] == 2 || data[0] == 3) {
        return Some(AudioFormat::DssSp);
    }
    if matches!(&data[..4], b"\x03ds2" | b"\x01ds2" | b"\x07ds2") && data.len() > 0x604 {
        let header_size = detect_ds2_audio_start(data);
        if data.len() <= header_size + 4 {
            return None;
        }
        let format_type = detect_ds2_format_type(data, header_size);
        return Some(match format_type {
            7 => AudioFormat::Ds2Qp7,
            6 => AudioFormat::Ds2Qp,
            _ => AudioFormat::Ds2Sp,
        });
    }
    None
}
