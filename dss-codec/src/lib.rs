pub mod bitstream;
pub mod codec;
pub mod crypto;
pub mod demux;
pub mod error;
pub mod output;
pub mod streaming;
pub mod tables;

use crate::crypto::ds2_encrypted::ENCRYPTED_MAGIC;
use crate::demux::{detect_format, AudioFormat};
use crate::error::{DecodeError, Result};
use crate::output::resample::resample;
use crate::output::wav::write_wav;
use crate::output::OutputConfig;
use crate::streaming::{DecryptStreamer, DecryptingDecoderStreamer};

use std::path::Path;

/// Decoded audio buffer
pub struct AudioBuffer {
    /// Samples as f64 (mono)
    pub samples: Vec<f64>,
    /// Native sample rate before any resampling
    pub native_rate: u32,
    /// Detected format
    pub format: AudioFormat,
    /// Runs of samples that one block format produced, in order
    pub parts: Vec<AudioPart>,
}

/// A run of samples that one block format produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPart {
    /// Index of the part's first sample in the buffer
    pub start: usize,
    /// Number of samples in the part
    pub len: usize,
    /// Sample rate of the part's samples
    pub rate: u32,
    /// Format of the part's frames
    pub format: AudioFormat,
    /// Format byte of the part's audio blocks
    pub block_format: u8,
}

/// Lightweight file/container inspection result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// Detected audio format.
    pub format: AudioFormat,
    /// Encryption metadata for the container.
    pub encryption: EncryptionInfo,
}

impl FileInfo {
    pub fn native_rate(&self) -> u32 {
        self.format.native_sample_rate()
    }
}

/// Encryption metadata detected from the file/container header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionInfo {
    None,
    EncryptedDs2Aes128,
    EncryptedDs2Aes256,
    EncryptedUnknown(u16),
}

/// Normalize a DSS/DS2 file to plain container bytes.
///
/// Plain DSS/DS2 input is returned unchanged. 
/// Encrypted DS2 input is decrypted to plain DS2
pub fn decrypt_file(path: &Path, password: Option<&[u8]>) -> Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    decrypt_to_bytes(&data, password)
}

/// Normalize raw DSS/DS2 bytes to plain container bytes.
///
/// Plain DSS/DS2 input is returned unchanged.
/// Encrypted DS2 input is decrypted to plain DS2
pub fn decrypt_to_bytes(data: &[u8], password: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut decryptor = DecryptStreamer::new(password);
    let mut plain = decryptor.push(data)?;
    plain.extend(decryptor.finish()?);
    Ok(plain)
}

/// Inspect a DSS/DS2 file and report format/encryption metadata.
pub fn inspect_file(path: &Path) -> Result<FileInfo> {
    let data = std::fs::read(path)?;
    inspect_bytes(&data)
}

/// Inspect raw DSS/DS2 bytes and report format/encryption metadata.
pub fn inspect_bytes(data: &[u8]) -> Result<FileInfo> {
    let format = detect_format(data)
        .ok_or_else(|| DecodeError::UnsupportedFormat(data.first().copied().unwrap_or(0)))?;

    let encryption = if data.starts_with(&ENCRYPTED_MAGIC) {
        let mode_offset = crate::crypto::ds2_encrypted::DECRYPT_DESCRIPTOR_OFFSET;
        let mode = data
            .get(mode_offset..mode_offset + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .ok_or_else(|| DecodeError::EncryptedDs2("missing decrypt descriptor".to_string()))?;

        match mode {
            1 => EncryptionInfo::EncryptedDs2Aes128,
            2 => EncryptionInfo::EncryptedDs2Aes256,
            other => EncryptionInfo::EncryptedUnknown(other),
        }
    } else {
        EncryptionInfo::None
    };

    Ok(FileInfo { format, encryption })
}

/// Decode a DSS/DS2 file to an AudioBuffer.
pub fn decode_file(path: &Path) -> Result<AudioBuffer> {
    decode_file_with_password(path, None)
}

/// Decode a DSS/DS2 file to an AudioBuffer, optionally decrypting encrypted DS2 input first.
pub fn decode_file_with_password(path: &Path, password: Option<&[u8]>) -> Result<AudioBuffer> {
    let data = std::fs::read(path)?;
    decode_to_buffer_with_password(&data, password)
}

/// Decode raw file bytes to an AudioBuffer.
pub fn decode_to_buffer(data: &[u8]) -> Result<AudioBuffer> {
    decode_to_buffer_with_password(data, None)
}

/// Decode raw file bytes to an AudioBuffer, optionally decrypting encrypted DS2 input first.
pub fn decode_to_buffer_with_password(data: &[u8], password: Option<&[u8]>) -> Result<AudioBuffer> {
    let mut decoder = DecryptingDecoderStreamer::new(password);
    let mut samples = decoder.push(data)?;
    samples.extend(decoder.finish_lenient()?);

    let format = decoder
        .format()
        .or_else(|| detect_format(data))
        .ok_or_else(|| DecodeError::UnsupportedFormat(data.first().copied().unwrap_or(0)))?;

    Ok(AudioBuffer {
        samples,
        native_rate: format.native_sample_rate(),
        format,
        parts: decoder.parts().to_vec(),
    })
}

/// Decode a file and write to WAV with given output configuration.
pub fn decode_and_write(
    input: &Path, 
    output: &Path, 
    config: &OutputConfig,
) -> Result<AudioBuffer> {
    decode_and_write_with_password(input, output, config, None)
}

pub fn decode_and_write_with_password(
    input: &Path,
    output: &Path,
    config: &OutputConfig,
    password: Option<&[u8]>,
) -> Result<AudioBuffer> {
    let mut buf = decode_file_with_password(input, password)?;

    let target_rate = config.sample_rate.unwrap_or(buf.native_rate);

    if buf.parts.len() > 1 {
        let mut samples = Vec::new();
        for part in &mut buf.parts {
            let part_samples = &buf.samples[part.start..part.start + part.len];
            let part_samples = if part.rate == target_rate {
                part_samples.to_vec()
            } else {
                resample(part_samples, part.rate, target_rate)?
            };
            part.start = samples.len();
            part.len = part_samples.len();
            part.rate = target_rate;
            samples.extend(part_samples);
        }
        buf.samples = samples;
    } else if target_rate != buf.native_rate {
        buf.samples = resample(&buf.samples, buf.native_rate, target_rate)?;
        for part in &mut buf.parts {
            part.len = buf.samples.len();
            part.rate = target_rate;
        }
    }

    write_wav(
        output,
        &buf.samples,
        target_rate,
        config.bit_depth,
        config.channels,
    )?;

    Ok(buf)
}

#[cfg(test)]
mod inspection_tests {
    use super::*;
    use crate::crypto::ds2_encrypted::DECRYPT_DESCRIPTOR_OFFSET;

    fn encrypted_ds2(mode: u16, format_type: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x800];
        data[..4].copy_from_slice(&ENCRYPTED_MAGIC);
        data[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + 2]
            .copy_from_slice(&mode.to_le_bytes());
        data[0x602] = 1;
        data[0x603] = 0xff;
        data[0x604] = format_type;
        data
    }

    #[test]
    fn inspect_bytes_reports_encryption_and_format() {
        assert_eq!(
            inspect_bytes(&encrypted_ds2(1, 6)).unwrap(),
            FileInfo {
                format: AudioFormat::Ds2Qp,
                encryption: EncryptionInfo::EncryptedDs2Aes128,
            }
        );
        assert_eq!(
            inspect_bytes(&encrypted_ds2(2, 7)).unwrap(),
            FileInfo {
                format: AudioFormat::Ds2Qp7,
                encryption: EncryptionInfo::EncryptedDs2Aes256,
            }
        );
        assert_eq!(
            inspect_bytes(&encrypted_ds2(99, 0)).unwrap().encryption,
            EncryptionInfo::EncryptedUnknown(99)
        );
    }

    #[test]
    fn inspect_bytes_reads_an_encrypted_file_as_its_plain_twin() {
        for (first, byte3, frames, format) in [
            (2u8, 0xffu8, 0u8, AudioFormat::Ds2Qp),
            (4, 0xff, 0, AudioFormat::Ds2Qp),
            (0, 0xff, 0, AudioFormat::Ds2Qp),
            (3, 0xff, 1, AudioFormat::Ds2Qp),
            (8, 0xff, 1, AudioFormat::Ds2Qp),
            (7, 0, 1, AudioFormat::Ds2Qp),
            (2, 0, 1, AudioFormat::Ds2Qp),
        ] {
            let mut data = encrypted_ds2(1, first);
            data[0x602] = frames;
            data[0x603] = byte3;
            data.resize(0xa00, 0);
            data[0x802] = 1;
            data[0x803] = 0xff;
            data[0x804] = 6;
            let mut plain = data.clone();
            plain[..4].copy_from_slice(b"\x03ds2");
            assert_eq!(
                detect_format(&plain),
                Some(format),
                "plain, {first} then QP"
            );
            let info = inspect_bytes(&data).unwrap();
            assert_eq!(info.format, format, "{first} then QP");
            assert_eq!(info.native_rate(), 16000);
        }
    }
}
