use crate::codec::ds2_qp::Ds2QpDecoder;
use crate::codec::ds2_sp::Ds2SpDecoder;
use crate::codec::dss_sp::DssSpDecoder;
use crate::crypto::ds2_encrypted::{EncryptedDs2BlockDecryptor, ENCRYPTED_MAGIC};
use crate::demux::ds2::{Ds2QpStreamDemuxer, Ds2SpStreamDemuxer};
use crate::demux::dss::DssSpStreamDemuxer;
use crate::demux::{detect_format, AudioFormat};
use crate::error::{DecodeError, Result};

pub struct StreamingDecoder {
    prebuffer: Vec<u8>,
    format: Option<AudioFormat>,
    demuxer: Option<ActiveDemuxer>,
    decoder: Option<ActiveDecoder>,
    finished: bool,
}

pub struct DecryptStreamer {
    password: Option<Vec<u8>>,
    prebuffer: Vec<u8>,
    mode: DecryptMode,
}

pub struct DecryptingDecoderStreamer {
    decryptor: DecryptStreamer,
    inner: StreamingDecoder,
    finished: bool,
}

enum DecryptMode {
    Unknown,
    PassThrough,
    Encrypted(EncryptedDs2BlockDecryptor),
}

enum ActiveDemuxer {
    Dss(DssSpStreamDemuxer),
    Ds2Sp(Ds2SpStreamDemuxer),
    Ds2Qp(Ds2QpStreamDemuxer),
}

enum ActiveDecoder {
    Dss(DssSpDecoder),
    Ds2Sp(Ds2SpDecoder),
    Ds2Qp(Ds2QpDecoder),
}

impl StreamingDecoder {
    pub fn new() -> Self {
        Self {
            prebuffer: Vec::new(),
            format: None,
            demuxer: None,
            decoder: None,
            finished: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if self.finished {
            return Err(DecodeError::AlreadyFinished);
        }

        if self.format.is_none() {
            self.prebuffer.extend_from_slice(bytes);
            if !self.try_initialize()? {
                return Ok(Vec::new());
            }

            let buffered = std::mem::take(&mut self.prebuffer);
            return self.push_active(&buffered);
        }

        self.push_active(bytes)
    }

    pub fn finish(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        if self.format.is_none() {
            if self.prebuffer.is_empty() {
                self.finished = true;
                return Ok(Vec::new());
            }

            if self.try_initialize()? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active()?);
                self.finished = true;
                return Ok(samples);
            }

            return if self.prebuffer.len() >= 4 && self.prebuffer[..4] == *b"\x03ds2" {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4
                && self.prebuffer[1..4] == *b"dss"
                && (self.prebuffer[0] == 2 || self.prebuffer[0] == 3)
            {
                Err(DecodeError::Truncated("DSS header".to_string()))
            } else {
                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            };
        }

        let samples = self.finish_active()?;
        self.finished = true;
        Ok(samples)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        if self.format.is_none() {
            if self.prebuffer.is_empty() {
                self.finished = true;
                return Ok(Vec::new());
            }

            if self.try_initialize()? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active_lenient()?);
                self.finished = true;
                return Ok(samples);
            }

            return if self.prebuffer.len() >= 4 && self.prebuffer[..4] == *b"\x03ds2" {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4
                && self.prebuffer[1..4] == *b"dss"
                && (self.prebuffer[0] == 2 || self.prebuffer[0] == 3)
            {
                Err(DecodeError::Truncated("DSS header".to_string()))
            } else {
                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            };
        }

        let samples = self.finish_active_lenient()?;
        self.finished = true;
        Ok(samples)
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    pub fn native_rate(&self) -> Option<u32> {
        self.format.map(|fmt| fmt.native_sample_rate())
    }

    fn try_initialize(&mut self) -> Result<bool> {
        if let Some(format) = detect_format(&self.prebuffer) {
            self.initialize_for_format(format);
            return Ok(true);
        }

        if self.prebuffer.len() >= 4 {
            let is_dss_prefix = self.prebuffer[1..4] == *b"dss"
                && (self.prebuffer[0] == 2 || self.prebuffer[0] == 3);
            let is_ds2_prefix = self.prebuffer[..4] == *b"\x03ds2";
            if !is_dss_prefix && !is_ds2_prefix {
                return Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ));
            }
        }

        Ok(false)
    }

    fn initialize_for_format(&mut self, format: AudioFormat) {
        self.format = Some(format);
        match format {
            AudioFormat::DssSp => {
                let version = self.prebuffer[0];
                self.demuxer = Some(ActiveDemuxer::Dss(DssSpStreamDemuxer::new(version)));
                self.decoder = Some(ActiveDecoder::Dss(DssSpDecoder::new()));
            }
            AudioFormat::Ds2Sp => {
                self.demuxer = Some(ActiveDemuxer::Ds2Sp(Ds2SpStreamDemuxer::new()));
                self.decoder = Some(ActiveDecoder::Ds2Sp(Ds2SpDecoder::new()));
            }
            AudioFormat::Ds2Qp => {
                self.demuxer = Some(ActiveDemuxer::Ds2Qp(Ds2QpStreamDemuxer::new()));
                self.decoder = Some(ActiveDecoder::Ds2Qp(Ds2QpDecoder::new()));
            }
        }
    }

    fn push_active(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.push(bytes)?,
            Some(ActiveDemuxer::Ds2Sp(demuxer)) => demuxer.push(bytes)?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.push(bytes)?,
            None => return Ok(Vec::new()),
        };

        self.decode_frames(frames)
    }

    fn finish_active(&mut self) -> Result<Vec<f64>> {
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish()?,
            Some(ActiveDemuxer::Ds2Sp(demuxer)) => demuxer.finish()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.finish()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn finish_active_lenient(&mut self) -> Result<Vec<f64>> {
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish_lenient()?,
            Some(ActiveDemuxer::Ds2Sp(demuxer)) => demuxer.finish_lenient()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.finish_lenient()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn decode_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<Vec<f64>> {
        let mut samples = Vec::new();
        match self.decoder.as_mut() {
            Some(ActiveDecoder::Dss(decoder)) => {
                for frame in frames {
                    let frame_samples = decoder.decode_frame(&frame);
                    samples.extend(frame_samples.into_iter().map(|sample| sample as f64));
                }
            }
            Some(ActiveDecoder::Ds2Sp(decoder)) => {
                for frame in frames {
                    samples.extend_from_slice(&decoder.decode_frame(&frame));
                }
            }
            Some(ActiveDecoder::Ds2Qp(decoder)) => {
                for frame in frames {
                    samples.extend_from_slice(&decoder.decode_frame(&frame));
                }
            }
            None => {}
        }

        Ok(samples)
    }
}

impl DecryptStreamer {
    pub fn new(password: Option<&[u8]>) -> Self {
        Self {
            password: password.map(|value| value.to_vec()),
            prebuffer: Vec::new(),
            mode: DecryptMode::Unknown,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::PassThrough => Ok(bytes.to_vec()),
            DecryptMode::Encrypted(decryptor) => decryptor.push(bytes),
            DecryptMode::Unknown => {
                self.prebuffer.extend_from_slice(bytes);
                if self.prebuffer.len() < 4 {
                    return Ok(Vec::new());
                }

                if self.prebuffer.starts_with(&ENCRYPTED_MAGIC) {
                    let password = self.password.as_deref().ok_or_else(|| {
                        DecodeError::EncryptedDs2(
                            "password required for encrypted DS2 input".to_string(),
                        )
                    })?;
                    let mut decryptor = EncryptedDs2BlockDecryptor::new(password);
                    let buffered = std::mem::take(&mut self.prebuffer);
                    let plain = decryptor.push(&buffered)?;
                    self.mode = DecryptMode::Encrypted(decryptor);
                    return Ok(plain);
                }

                if is_plain_prefix(&self.prebuffer) {
                    let buffered = std::mem::take(&mut self.prebuffer);
                    self.mode = DecryptMode::PassThrough;
                    return Ok(buffered);
                }

                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            }
        }
    }

    pub fn finish(&mut self) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::PassThrough => Ok(std::mem::take(&mut self.prebuffer)),
            DecryptMode::Encrypted(decryptor) => decryptor.finish(),
            DecryptMode::Unknown => {
                if self.prebuffer.is_empty() {
                    return Ok(Vec::new());
                }

                if ENCRYPTED_MAGIC.starts_with(&self.prebuffer) {
                    return Err(DecodeError::Truncated("encrypted DS2 header".to_string()));
                }

                if self.prebuffer.len() >= 4 && !is_plain_prefix(&self.prebuffer) {
                    return Err(DecodeError::UnsupportedFormat(
                        self.prebuffer.first().copied().unwrap_or(0),
                    ));
                }

                Ok(std::mem::take(&mut self.prebuffer))
            }
        }
    }
}

impl DecryptingDecoderStreamer {
    pub fn new(password: Option<&[u8]>) -> Self {
        Self {
            decryptor: DecryptStreamer::new(password),
            inner: StreamingDecoder::new(),
            finished: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if self.finished {
            return Err(DecodeError::AlreadyFinished);
        }

        let plain = self.decryptor.push(bytes)?;
        self.inner.push(&plain)
    }

    pub fn finish(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let plain = self.decryptor.finish()?;
        let mut samples = self.inner.push(&plain)?;
        samples.extend(self.inner.finish()?);
        self.finished = true;
        Ok(samples)
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.inner.format()
    }

    pub fn native_rate(&self) -> Option<u32> {
        self.inner.native_rate()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let plain = self.decryptor.finish()?;
        let mut samples = self.inner.push(&plain)?;
        samples.extend(self.inner.finish_lenient()?);
        self.finished = true;
        Ok(samples)
    }
}

impl Default for DecryptingDecoderStreamer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Default for StreamingDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn is_plain_prefix(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x03ds2")
        || (bytes.len() >= 4 && bytes[1..4] == *b"dss" && (bytes[0] == 2 || bytes[0] == 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_header_only(mode: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x600 + 512];
        data[..4].copy_from_slice(b"\x03ds2");
        data[0x600 + 4] = mode;
        data
    }

    #[test]
    fn test_streaming_decoder_detects_only_when_enough_bytes_arrive() {
        let data = make_ds2_header_only(6);
        let mut decoder = StreamingDecoder::new();

        let first = decoder.push(&data[..4]).unwrap();
        assert!(first.is_empty());
        assert_eq!(decoder.format(), None);
        assert_eq!(decoder.native_rate(), None);

        let second = decoder.push(&data[4..]).unwrap();
        assert!(second.is_empty());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        assert_eq!(decoder.native_rate(), Some(16000));
    }

    #[test]
    fn test_streaming_decoder_truncated_header_on_finish() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(b"\x03ds2").unwrap();

        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_streaming_decoder_push_after_finish_errors() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.finish().unwrap();

        let err = decoder.push(b"\x03ds2").unwrap_err();
        assert!(matches!(err, DecodeError::AlreadyFinished));
    }

    #[test]
    fn decrypt_streamer_passes_plain_ds2_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x03ds2rest").unwrap();
        assert_eq!(plain, b"\x03ds2rest");
        assert!(decryptor.finish().unwrap().is_empty());
    }

    #[test]
    fn decrypt_streamer_passes_plain_dss_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x02dssrest").unwrap();
        assert_eq!(plain, b"\x02dssrest");
        assert!(decryptor.finish().unwrap().is_empty());
    }

    #[test]
    fn decrypt_streamer_rejects_unsupported_prefix() {
        let mut decryptor = DecryptStreamer::new(None);
        let err = decryptor.push(b"nope").unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedFormat(_)));
    }

    #[test]
    fn decrypting_decoder_streamer_plain_ds2_decodes() {
        let data = make_ds2_header_only(6);
        let mut decoder = DecryptingDecoderStreamer::new(None);
        let samples = decoder.push(&data).unwrap();
        assert!(samples.is_empty());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        assert_eq!(decoder.native_rate(), Some(16000));
        assert!(decoder.finish().unwrap().is_empty());
    }

    #[test]
    fn decrypting_decoder_streamer_push_after_finish_errors() {
        let mut decoder = DecryptingDecoderStreamer::new(None);
        let _ = decoder.finish().unwrap();
        let err = decoder.push(b"\x03ds2").unwrap_err();
        assert!(matches!(err, DecodeError::AlreadyFinished));
    }
}
