pub mod bitstream;
pub mod codec;
pub mod crypto;
pub mod demux;
pub mod error;
pub mod output;
pub mod streaming;
pub mod tables;

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

    if target_rate != buf.native_rate {
        buf.samples = resample(&buf.samples, buf.native_rate, target_rate)?;
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
