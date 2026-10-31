use crate::codec::ds2_qp::Ds2QpDecoder;
use crate::codec::grundig_sp::GrundigSpDecoder;
use crate::crypto::ds2_encrypted::{EncryptedDs2BlockDecryptor, ENCRYPTED_MAGIC};
use crate::demux::ds2::{
    demux_ds2, ds2_layout, known_ds2_format_type, mixed_qp_frames, qp_frame_in_progress,
    DemuxedDs2, Ds2Layout, Ds2Qp7Segment, Ds2QpStreamDemuxer,
};
use crate::demux::dss::{DssSpStreamDemuxer, TaggedFrames};
use crate::demux::grundig::GrundigSpStreamDemuxer;
use crate::demux::{
    audio_start, check_sp_codec, detect_format, ds2_header_size, dss_header_size, sp_codec,
    AudioFormat,
};
use crate::error::{DecodeError, Result};
use crate::AudioPart;

pub struct StreamingDecoder {
    prebuffer: Vec<u8>,
    raw_buf: Vec<u8>,
    format: Option<AudioFormat>,
    demuxer: Option<ActiveDemuxer>,
    decoder: Option<ActiveDecoder>,
    block_format: u8,
    parts: Vec<AudioPart>,
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
    Ds2Qp(Ds2QpStreamDemuxer),
    Grundig(GrundigSpStreamDemuxer),
}

enum ActiveDecoder {
    Dss(GrundigSpDecoder),
    Ds2Qp(Ds2QpDecoder),
    Grundig(GrundigSpDecoder),
}

impl StreamingDecoder {
    pub fn new() -> Self {
        Self {
            prebuffer: Vec::new(),
            raw_buf: Vec::new(),
            format: None,
            demuxer: None,
            decoder: None,
            block_format: 0,
            parts: Vec::new(),
            finished: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if self.finished {
            return Err(DecodeError::AlreadyFinished);
        }

        if self.format.is_none() {
            self.prebuffer.extend_from_slice(bytes);
            if !self.try_initialize(false)? {
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

            if self.try_initialize(true)? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active()?);
                self.finished = true;
                return Ok(samples);
            }

            return if ds2_header_size(&self.prebuffer).is_some() {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4 && self.prebuffer[1..4] == *b"dss" {
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

            if self.try_initialize(true)? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active_lenient()?);
                self.finished = true;
                return Ok(samples);
            }

            return if ds2_header_size(&self.prebuffer).is_some() {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4 && self.prebuffer[1..4] == *b"dss" {
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
        self.parts.first().map(|part| part.format).or(self.format)
    }

    pub fn native_rate(&self) -> Option<u32> {
        self.format().map(|fmt| fmt.native_sample_rate())
    }

    /// Runs of samples that one block format produced, in output order.
    pub fn parts(&self) -> &[AudioPart] {
        &self.parts
    }

    fn try_initialize(&mut self, at_end: bool) -> Result<bool> {
        if let Some(format) = detect_format(&self.prebuffer) {
            let header_size = match format {
                AudioFormat::DssSp | AudioFormat::GrundigSp => dss_header_size(&self.prebuffer),
                _ => ds2_header_size(&self.prebuffer),
            };
            let codec_pending = header_size.is_some_and(|size| {
                let start = audio_start(&self.prebuffer, size);
                known_ds2_format_type(&self.prebuffer, start).is_none()
            });
            if codec_pending && !at_end {
                return Ok(false);
            }
            self.initialize_for_format(format)?;
            return Ok(true);
        }

        if self.prebuffer.len() >= 4 {
            let is_dss_prefix = self.prebuffer[1..4] == *b"dss";
            let is_ds2_prefix = ds2_header_size(&self.prebuffer).is_some();
            if !is_dss_prefix && !is_ds2_prefix {
                return Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ));
            }
        }

        Ok(false)
    }

    fn initialize_for_format(&mut self, format: AudioFormat) -> Result<()> {
        let mut codec = 0;
        if matches!(format, AudioFormat::DssSp | AudioFormat::Ds2Sp) {
            codec = sp_codec(&self.prebuffer).unwrap_or(0);
            check_sp_codec(codec)?;
        }
        let variable_rate = codec == 1;
        self.format = Some(format);
        self.block_format = match format {
            AudioFormat::Ds2Qp => 6,
            AudioFormat::Ds2Qp7 => 7,
            _ => codec,
        };
        match format {
            AudioFormat::DssSp => {
                let header_size = dss_header_size(&self.prebuffer).unwrap_or(512);
                self.demuxer = Some(ActiveDemuxer::Dss(DssSpStreamDemuxer::with_header_size(
                    audio_start(&self.prebuffer, header_size),
                    variable_rate,
                )));
                self.decoder = Some(ActiveDecoder::Dss(GrundigSpDecoder::with_variable_rate(
                    variable_rate,
                )));
            }
            AudioFormat::Ds2Sp => {
                let header_size = ds2_header_size(&self.prebuffer).unwrap_or(3 * 512);
                self.demuxer = Some(ActiveDemuxer::Dss(DssSpStreamDemuxer::with_header_size(
                    audio_start(&self.prebuffer, header_size),
                    variable_rate,
                )));
                self.decoder = Some(ActiveDecoder::Dss(GrundigSpDecoder::with_variable_rate(
                    variable_rate,
                )));
            }
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => {
                self.demuxer = Some(ActiveDemuxer::Ds2Qp(Ds2QpStreamDemuxer::new()));
                self.decoder = Some(ActiveDecoder::Ds2Qp(Ds2QpDecoder::new()));
            }
            AudioFormat::GrundigSp => {
                let header_size = dss_header_size(&self.prebuffer).unwrap_or(6 * 512);
                self.demuxer = Some(ActiveDemuxer::Grundig(GrundigSpStreamDemuxer::new(
                    audio_start(&self.prebuffer, header_size),
                )));
                self.decoder = Some(ActiveDecoder::Grundig(GrundigSpDecoder::new()));
            }
        }
        Ok(())
    }

    fn push_active(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            self.raw_buf.extend_from_slice(bytes);
            return Ok(Vec::new());
        }

        let tag = self.block_format;
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.push(bytes)?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => tagged(tag, demuxer.push(bytes)?),
            Some(ActiveDemuxer::Grundig(demuxer)) => tagged(tag, demuxer.push(bytes)?),
            None => return Ok(Vec::new()),
        };

        self.decode_frames(frames)
    }

    fn finish_active(&mut self) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            return self.decode_buffered_ds2_qp(true);
        }

        let tag = self.block_format;
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => tagged(tag, demuxer.finish()?),
            Some(ActiveDemuxer::Grundig(demuxer)) => demuxer.finish()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn finish_active_lenient(&mut self) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            return self.decode_buffered_ds2_qp(false);
        }

        let tag = self.block_format;
        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish_lenient()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => tagged(tag, demuxer.finish_lenient()?),
            Some(ActiveDemuxer::Grundig(demuxer)) => demuxer.finish_lenient()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn decode_buffered_ds2_qp(&mut self, strict: bool) -> Result<Vec<f64>> {
        let start = ds2_header_size(&self.raw_buf).map(|size| audio_start(&self.raw_buf, size));
        let layout = start.map(|start| ds2_layout(&self.raw_buf, start));
        if let (Some(start), Some(Ds2Layout::Walk)) = (start, layout) {
            let mut demuxer = DssSpStreamDemuxer::with_format(start, self.block_format);
            let mut frames = demuxer.push(&self.raw_buf)?;
            frames.extend(if strict {
                demuxer.finish()?
            } else {
                demuxer.finish_lenient()?
            });
            return self.decode_frames(frames);
        }

        if strict && qp_frame_in_progress(&self.raw_buf) {
            return Err(DecodeError::Truncated("DS2 QP frame".to_string()));
        }
        if let (Some(start), Some(Ds2Layout::MixedQp)) = (start, layout) {
            let (frames, _cut) = mixed_qp_frames(&self.raw_buf, start);
            return self.decode_frames(frames);
        }

        let demuxed = demux_ds2(&self.raw_buf)?;
        let samples = match (demuxed, self.decoder.as_mut()) {
            (DemuxedDs2::QpSegments { segments, .. }, Some(ActiveDecoder::Ds2Qp(decoder))) => {
                decoder.decode_qp_segments(&segments)
            }
            (DemuxedDs2::Qp7Segments { segments, .. }, Some(ActiveDecoder::Ds2Qp(decoder))) => {
                decoder.decode_qp7_segments(&segments)
            }
            _ => Vec::new(),
        };
        if !samples.is_empty() {
            self.start_part(self.block_format);
            self.add_to_part(samples.len());
        }
        Ok(samples)
    }

    fn start_part(&mut self, tag: u8) {
        if !self.parts.is_empty() || decoder_kind(tag) != decoder_kind(self.block_format) {
            self.decoder = Some(match tag {
                6 | 7 => ActiveDecoder::Ds2Qp(Ds2QpDecoder::new()),
                _ => ActiveDecoder::Dss(GrundigSpDecoder::with_variable_rate(tag == 1)),
            });
        }
        let format = match (tag, self.format) {
            (6, _) => AudioFormat::Ds2Qp,
            (7, _) => AudioFormat::Ds2Qp7,
            (_, Some(AudioFormat::DssSp)) => AudioFormat::DssSp,
            (1, Some(AudioFormat::GrundigSp)) => AudioFormat::DssSp,
            (_, Some(AudioFormat::GrundigSp)) => AudioFormat::GrundigSp,
            _ => AudioFormat::Ds2Sp,
        };
        let start = self.parts.last().map_or(0, |part| part.start + part.len);
        self.parts.push(AudioPart {
            start,
            len: 0,
            rate: format.native_sample_rate(),
            format,
            block_format: tag,
        });
    }

    fn add_to_part(&mut self, len: usize) {
        if let Some(part) = self.parts.last_mut() {
            part.len += len;
        }
    }

    fn decode_frames(&mut self, frames: TaggedFrames) -> Result<Vec<f64>> {
        let mut samples = Vec::new();
        for (tag, frame) in frames {
            if self.parts.last().is_none_or(|part| part.block_format != tag) {
                self.start_part(tag);
            }
            let before = samples.len();
            match self.decoder.as_mut() {
                Some(ActiveDecoder::Dss(decoder)) => {
                    let frame_samples = decoder.decode_dss_packet(&frame);
                    samples.extend(frame_samples.into_iter().map(|sample| sample as f64));
                }
                Some(ActiveDecoder::Ds2Qp(decoder)) if tag == 7 => {
                    samples.extend(decoder.decode_qp7_segments(&[Ds2Qp7Segment {
                        records: vec![frame],
                        total_frames: 1,
                        reset_before: false,
                    }]));
                }
                Some(ActiveDecoder::Ds2Qp(decoder)) => {
                    samples.extend_from_slice(&decoder.decode_frame(&frame));
                }
                Some(ActiveDecoder::Grundig(decoder)) => {
                    let frame_samples = decoder.decode_frame(&frame);
                    samples.extend(frame_samples.into_iter().map(|sample| sample as f64));
                }
                None => {}
            }
            self.add_to_part(samples.len() - before);
        }

        Ok(samples)
    }
}

fn decoder_kind(tag: u8) -> u8 {
    if tag == 7 {
        6
    } else {
        tag
    }
}

fn tagged(tag: u8, frames: Vec<Vec<u8>>) -> TaggedFrames {
    frames.into_iter().map(|frame| (tag, frame)).collect()
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

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::Encrypted(decryptor) => decryptor.finish_lenient(),
            _ => self.finish(),
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

        let plain = self.decryptor.finish_lenient()?;
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

    /// Runs of samples that one block format produced, in output order.
    pub fn parts(&self) -> &[AudioPart] {
        self.inner.parts()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let plain = self.decryptor.finish_lenient()?;
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
    ds2_header_size(bytes).is_some() || (bytes.len() >= 4 && bytes[1..4] == *b"dss")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_header_only(mode: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x600 + 512];
        data[..4].copy_from_slice(b"\x03ds2");
        data[0x600 + 1] = 3;
        // detect_ds2_format_type only trusts a block's mode byte when its
        // frame count is non-zero, as in every real recording
        data[0x600 + 2] = 1;
        data[0x600 + 3] = 0xff;
        data[0x600 + 4] = mode;
        data
    }

    fn sp_block(frame_count: u8) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[1] = 3;
        block[2] = frame_count;
        block[3] = 0xff;
        for (i, byte) in block[6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        block
    }

    fn sp_file(prefix: &[u8; 4], header_size: usize, codec: u8) -> Vec<u8> {
        let mut data = vec![0u8; header_size];
        data[..4].copy_from_slice(prefix);
        let mut block = sp_block(3);
        block[4] = codec;
        data.extend_from_slice(&block);
        data
    }

    fn stream_decode(data: &[u8], chunk: usize) -> Result<(Vec<f64>, Option<AudioFormat>)> {
        let mut decoder = StreamingDecoder::new();
        let mut samples = Vec::new();
        for part in data.chunks(chunk) {
            samples.extend(decoder.push(part)?);
        }
        samples.extend(decoder.finish()?);
        Ok((samples, decoder.format()))
    }

    #[test]
    fn test_streaming_decoder_ds2_sp_decodes_like_dss_sp() {
        let ds2 = sp_file(b"\x03ds2", 0x600, 0);
        let dss = sp_file(b"\x02dss", 0x400, 0);
        let (ds2_samples, ds2_format) = stream_decode(&ds2, 100).unwrap();
        let (dss_samples, dss_format) = stream_decode(&dss, 100).unwrap();
        assert_eq!(ds2_format, Some(AudioFormat::Ds2Sp));
        assert_eq!(dss_format, Some(AudioFormat::DssSp));
        assert_eq!(ds2_samples.len(), 3 * 288);
        assert_eq!(ds2_samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&ds2).unwrap().samples, dss_samples);
    }

    #[test]
    fn test_streaming_decoder_variable_rate_matches_batch() {
        let dss = sp_file(b"\x02dss", 0x400, 1);
        let ds2 = sp_file(b"\x03ds2", 0x600, 1);
        let (dss_samples, dss_format) = stream_decode(&dss, 100).unwrap();
        let (ds2_samples, ds2_format) = stream_decode(&ds2, 1).unwrap();
        assert_eq!(dss_format, Some(AudioFormat::DssSp));
        assert_eq!(ds2_format, Some(AudioFormat::Ds2Sp));
        assert_eq!(dss_samples.len(), 3 * 288);
        assert_eq!(ds2_samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&dss).unwrap().samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&ds2).unwrap().samples, dss_samples);

        let (fixed, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 0), 100).unwrap();
        assert_ne!(fixed, dss_samples);
    }

    #[test]
    fn test_streaming_decoder_cut_qp7_keeps_last_frame() {
        let mut data = make_ds2_header_only(7);
        data.truncate(0x600 + 6 + 13);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 2;
        data[0x600 + 5] = 0xff;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        data[0x600 + 7] &= 0x7f;
        let err = stream_decode(&data, 1).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 QP frame"));
        let mut decoder = StreamingDecoder::new();
        let mut samples = decoder.push(&data).unwrap();
        samples.extend(decoder.finish_lenient().unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp7));
        assert_eq!(samples.len(), 2 * 256);
        assert_eq!(crate::decode_to_buffer(&data).unwrap().samples, samples);
    }

    #[test]
    fn test_streaming_decoder_qp_strict_finish_accepts_cut_after_declared_frames() {
        let data = make_ds2_header_only(6);
        let (expected, _) = stream_decode(&data, 64).unwrap();
        assert_eq!(expected.len(), 256);
        for end in [0x600 + 6 + 56, 0x600 + 300, data.len() - 1] {
            let (samples, format) = stream_decode(&data[..end], 64).unwrap();
            assert_eq!(format, Some(AudioFormat::Ds2Qp));
            assert_eq!(samples, expected);
        }
        let mut two = data.clone();
        two.extend_from_slice(&data[0x600..]);
        for end in [data.len() + 1, data.len() + 5] {
            let (samples, _) = stream_decode(&two[..end], 64).unwrap();
            assert_eq!(samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_qp_strict_finish_rejects_frame_into_cut_block_header() {
        let mut data = make_ds2_header_only(6);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 10;
        data[0x600 + 3] = 0xff;
        data[0x600 + 5] = 0xff;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut next = data[0x600..].to_vec();
        next[1] = 30;
        next[2] = 1;
        data.extend_from_slice(&next);
        let whole = 0x600 + 512;
        for end in [whole + 1, whole + 5, whole + 6, whole + 30] {
            let cut = &data[..end];
            let err = stream_decode(cut, 64).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 QP frame"));
            let mut decoder = StreamingDecoder::new();
            let mut lenient = decoder.push(cut).unwrap();
            lenient.extend(decoder.finish_lenient().unwrap());
            assert_eq!(lenient.len(), 10 * 256);
            assert_eq!(crate::decode_to_buffer(cut).unwrap().samples, lenient);
        }
    }

    fn dss_with_header_blocks(first_byte: u8, codec: u8) -> Vec<u8> {
        let mut data = sp_file(b"\x02dss", usize::from(first_byte.max(1)) * 512, codec);
        data[0] = first_byte;
        data
    }

    #[test]
    fn test_streaming_decoder_reads_any_dss_header_block_count() {
        let (expected, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 0), 100).unwrap();
        assert_eq!(expected.len(), 3 * 288);
        for first_byte in [0u8, 1, 3, 8, 19, 255] {
            let data = dss_with_header_blocks(first_byte, 0);
            for chunk in [7, data.len()] {
                let (samples, format) = stream_decode(&data, chunk).unwrap();
                assert_eq!(format, Some(AudioFormat::DssSp));
                assert_eq!(samples, expected);
            }
            assert_eq!(crate::decode_to_buffer(&data).unwrap().samples, expected);
            let mut decoder = DecryptingDecoderStreamer::new(None);
            let mut samples = decoder.push(&data).unwrap();
            samples.extend(decoder.finish().unwrap());
            assert_eq!(samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_reads_variable_rate_sp_after_six_header_blocks() {
        let (expected, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 1), 100).unwrap();
        assert_eq!(expected.len(), 3 * 288);
        let data = dss_with_header_blocks(6, 1);
        for chunk in [7, 100, data.len()] {
            let (samples, format) = stream_decode(&data, chunk).unwrap();
            assert_eq!(format, Some(AudioFormat::DssSp));
            assert_eq!(samples, expected);
        }
        let buffer = crate::decode_to_buffer(&data).unwrap();
        assert_eq!(buffer.format, AudioFormat::DssSp);
        assert_eq!(buffer.native_rate, 12000);
        assert_eq!(buffer.samples, expected);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..6 * 512 + 5]).unwrap();
        assert_eq!(decoder.format(), None);

        let grundig = dss_with_header_blocks(6, 0);
        assert_eq!(stream_decode(&grundig, 7).unwrap().1, Some(AudioFormat::GrundigSp));
        let buffer = crate::decode_to_buffer(&grundig).unwrap();
        assert_eq!(buffer.format, AudioFormat::GrundigSp);
        assert_eq!(buffer.native_rate, 12000);
    }

    #[test]
    fn test_streaming_decoder_fixed_rate_sp_decodes_like_grundig() {
        let dss = sp_file(b"\x02dss", 0x400, 0);
        let grundig = dss_with_header_blocks(6, 0);
        let (dss_samples, dss_format) = stream_decode(&dss, 100).unwrap();
        let (grundig_samples, grundig_format) = stream_decode(&grundig, 100).unwrap();
        assert_eq!(dss_format, Some(AudioFormat::DssSp));
        assert_eq!(grundig_format, Some(AudioFormat::GrundigSp));
        assert_eq!(dss_samples.len(), 3 * 288);
        assert!(dss_samples.iter().any(|&sample| sample != 0.0));
        assert_eq!(dss_samples, grundig_samples);
        assert_eq!(crate::decode_to_buffer(&dss).unwrap().samples, grundig_samples);
    }

    fn assert_decodes_to_zeros(data: &[u8]) {
        for chunk in [7, 100, data.len()] {
            let (samples, _) = stream_decode(data, chunk).unwrap();
            assert_eq!(samples.len(), 3 * 288);
            assert!(samples.iter().all(|&sample| sample == 0.0));
        }
        let buffer = crate::decode_to_buffer(data).unwrap();
        assert_eq!(buffer.samples.len(), 3 * 288);
        assert!(buffer.samples.iter().all(|&sample| sample == 0.0));
    }

    #[test]
    fn test_streaming_decoder_zero_sp_payload_decodes_to_zeros() {
        for codec in [0u8, 1] {
            for first_byte in [3u8, 8, 19] {
                let mut data = dss_with_header_blocks(first_byte, codec);
                let header_size = usize::from(first_byte) * 512;
                data[0x56..header_size].fill(0xff);
                data[header_size + 6..].fill(0);
                assert_decodes_to_zeros(&data);
            }
            let mut data = sp_file(b"\x03ds2", 0x600, codec);
            data[0x600 + 6..].fill(0);
            assert_decodes_to_zeros(&data);
        }
    }

    fn stale_header_copy(first_byte: u8, codec: u8) -> Vec<u8> {
        let mut copy = vec![0xffu8; usize::from(first_byte) * 512];
        copy[..6].copy_from_slice(&[first_byte, b'd', b's', b's', codec, 0]);
        copy[512..518].copy_from_slice(&[0, 0, 0, 0, 0xff, 0xff]);
        copy
    }

    #[test]
    fn test_streaming_decoder_skips_stale_header_copy() {
        for (prefix, header_size, copy_codec, codec, what) in [
            (b"\x02dss", 0x400, 2u8, 1u8, "DSS header"),
            (b"\x06dss", 0xc00, 2, 1, "Grundig header"),
            (b"\x06dss", 0xc00, 1, 0, "Grundig header"),
            (b"\x03ds2", 0x600, 0, 1, "DS2 header"),
            (b"\x03ds2", 0x600, 1, 6, "DS2 header"),
        ] {
            let plain = sp_file(prefix, header_size, codec);
            let (expected, format) = stream_decode(&plain, 100).unwrap();
            assert!(!expected.is_empty());
            let mut data = plain[..header_size].to_vec();
            data.extend_from_slice(&stale_header_copy(prefix[0], copy_codec));
            data.extend_from_slice(&plain[header_size..]);
            for chunk in [7, 100, data.len()] {
                assert_eq!(stream_decode(&data, chunk).unwrap(), (expected.clone(), format));
            }
            assert_eq!(crate::decode_to_buffer(&data).unwrap().samples, expected);

            let cut = &data[..header_size + 512];
            assert!(matches!(stream_decode(cut, 7), Err(DecodeError::Truncated(w)) if w == what));
            assert!(matches!(
                crate::decode_to_buffer(cut),
                Err(DecodeError::Truncated(w)) if w == what
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_dss_long_header_cut_is_truncated() {
        for first_byte in [8u8, 19] {
            let data = dss_with_header_blocks(first_byte, 0);
            let cut = &data[..usize::from(first_byte) * 512 - 1];
            let err = stream_decode(cut, 64).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DSS header"));
            let mut decoder = StreamingDecoder::new();
            let _ = decoder.push(cut).unwrap();
            assert!(matches!(
                decoder.finish_lenient(),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(cut),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_other_codecs_after_any_dss_header() {
        for first_byte in [0u8, 6, 8, 19] {
            for codec in [2u8, 6] {
                let data = dss_with_header_blocks(first_byte, codec);
                let err = stream_decode(&data, 100).unwrap_err();
                assert!(matches!(err, DecodeError::UnsupportedCodec(c) if c == codec));
                assert!(matches!(
                    crate::decode_to_buffer(&data),
                    Err(DecodeError::UnsupportedCodec(c)) if c == codec
                ));
            }
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_lp() {
        for (prefix, header_size) in [(b"\x03ds2", 0x600), (b"\x02dss", 0x400)] {
            let data = sp_file(prefix, header_size, 2);
            let err = stream_decode(&data, 100).unwrap_err();
            assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
            let err = stream_decode(&data, data.len()).unwrap_err();
            assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
            assert!(matches!(
                crate::decode_to_buffer(&data),
                Err(DecodeError::UnsupportedCodec(2))
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_unknown_dss_codec() {
        let data = sp_file(b"\x02dss", 0x400, 5);
        let err = stream_decode(&data, 64).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(5)));
    }

    #[test]
    fn test_streaming_decoder_waits_for_dss_block_with_frames() {
        let mut data = sp_file(b"\x02dss", 0x400, 0);
        data[0x400 + 2] = 0;
        data.extend_from_slice(&sp_block(3));
        let mut decoder = StreamingDecoder::new();
        let mut samples = decoder.push(&data[..0x400 + 512 + 5]).unwrap();
        assert_eq!(decoder.format(), None);
        samples.extend(decoder.push(&data[0x400 + 512 + 5..]).unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::DssSp));
        samples.extend(decoder.finish().unwrap());
        assert_eq!(samples.len(), 3 * 288);
    }

    #[test]
    fn test_streaming_decoder_ignores_dss_header_codec_byte() {
        let data = sp_file(b"\x02dss", 0x400, 0);
        let (expected, _) = stream_decode(&data, 100).unwrap();
        for byte in [1u8, 2, 0xff] {
            let mut marked = data.clone();
            marked[0x2a4] = byte;
            let (samples, format) = stream_decode(&marked, 100).unwrap();
            assert_eq!(format, Some(AudioFormat::DssSp));
            assert_eq!(samples, expected);
            assert_eq!(crate::decode_to_buffer(&marked).unwrap().samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_after_empty_dss_block() {
        let mut data = sp_file(b"\x02dss", 0x400, 0);
        data[0x400 + 2] = 0;
        let mut block = sp_block(2);
        block[4] = 2;
        data.extend_from_slice(&block);
        let err = stream_decode(&data, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        assert!(matches!(
            crate::decode_to_buffer(&data),
            Err(DecodeError::UnsupportedCodec(2))
        ));
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_in_partial_dss_block() {
        let data = sp_file(b"\x02dss", 0x400, 2);
        let cut = &data[..0x400 + 6];
        let err = stream_decode(cut, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        let err = StreamingDecoder::new().push(cut).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        assert!(matches!(
            crate::decode_to_buffer(cut),
            Err(DecodeError::UnsupportedCodec(2))
        ));
    }

    #[test]
    fn test_streaming_decoder_dss_without_frames_ends_empty() {
        let mut data = sp_file(b"\x02dss", 0x400, 2);
        data[0x400 + 2] = 0;
        let (samples, format) = stream_decode(&data, 64).unwrap();
        assert!(samples.is_empty());
        assert_eq!(format, Some(AudioFormat::DssSp));
    }

    fn ds2_with_empty_first_block(codec: u8) -> Vec<u8> {
        let mut data = sp_file(b"\x03ds2", 0x600, 0);
        data[0x600 + 2] = 0;
        let mut block = sp_block(2);
        block[4] = codec;
        data.extend_from_slice(&block);
        data
    }

    #[test]
    fn test_streaming_decoder_waits_for_ds2_block_with_frames() {
        let data = ds2_with_empty_first_block(6);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x600 + 512]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x600 + 512..0x600 + 512 + 6]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn test_streaming_decoder_pause_block_sets_qp_format() {
        let mut data = make_ds2_header_only(6);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 0;
        data[0x600 + 5] = 3;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut counted = data.clone();
        counted[0x600 + 2] = 3;
        counted[0x600 + 5] = 0xff;

        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x600 + 5]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x600 + 5..0x600 + 6]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));

        let (expected, _) = stream_decode(&counted, 1).unwrap();
        assert_eq!(expected.len(), 3 * 256);
        for chunk in [1, 7, data.len()] {
            let (samples, format) = stream_decode(&data, chunk).unwrap();
            assert_eq!(format, Some(AudioFormat::Ds2Qp));
            assert_eq!(samples, expected);
        }
        let buffer = crate::decode_to_buffer(&data).unwrap();
        assert_eq!(buffer.format, AudioFormat::Ds2Qp);
        assert_eq!(buffer.native_rate, 16000);
        assert_eq!(buffer.samples, expected);
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_after_empty_ds2_block() {
        let data = ds2_with_empty_first_block(2);
        let err = stream_decode(&data, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
    }

    #[test]
    fn test_streaming_decoder_ds2_without_frames_ends_empty() {
        let mut data = sp_file(b"\x03ds2", 0x600, 2);
        data[0x600 + 2] = 0;
        let (samples, format) = stream_decode(&data, 64).unwrap();
        assert!(samples.is_empty());
        assert_eq!(format, Some(AudioFormat::Ds2Sp));
    }

    #[test]
    fn test_streaming_decoder_ds2_cut_inside_first_block_header() {
        for mode in [0u8, 6, 7] {
            let data = sp_file(b"\x03ds2", 0x600, mode);
            let cut = &data[..0x600 + 5];
            let err = stream_decode(cut, 7).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 header"));
            let mut decoder = StreamingDecoder::new();
            let _ = decoder.push(cut).unwrap();
            assert!(matches!(
                decoder.finish_lenient(),
                Err(DecodeError::Truncated(what)) if what == "DS2 header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(cut),
                Err(DecodeError::Truncated(what)) if what == "DS2 header"
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_dss_cut_inside_header() {
        let data = sp_file(b"\x02dss", 0x400, 0);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x400 - 1]).unwrap();
        assert!(matches!(
            decoder.finish(),
            Err(DecodeError::Truncated(what)) if what == "DSS header"
        ));
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x400 - 1]).unwrap();
        assert!(matches!(
            decoder.finish_lenient(),
            Err(DecodeError::Truncated(what)) if what == "DSS header"
        ));
    }

    #[test]
    fn test_streaming_decoder_grundig_strict_finish_reports_cut() {
        let mut whole = vec![0u8; 6 * 512];
        whole[..4].copy_from_slice(b"\x06dss");
        let mut block = vec![0u8; 512];
        block[..6].copy_from_slice(&[0x00, 3, 2, 0xff, 0x00, 0xff]);
        for (i, byte) in block[6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        whole.extend_from_slice(&block);

        let (samples, format) = stream_decode(&whole, 37).unwrap();
        assert_eq!(format, Some(AudioFormat::GrundigSp));
        assert!(!samples.is_empty());
        assert_eq!(crate::decode_to_buffer(&whole).unwrap().samples, samples);

        let data = &whole[..6 * 512 + 6 + 70];
        let err = stream_decode(data, 37).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));

        let mut decoder = StreamingDecoder::new();
        let mut lenient = Vec::new();
        for part in data.chunks(37) {
            lenient.extend(decoder.push(part).unwrap());
        }
        lenient.extend(decoder.finish_lenient().unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::GrundigSp));
        assert!(!lenient.is_empty());
        assert_eq!(crate::decode_to_buffer(data).unwrap().samples, lenient);

        let data = &whole[..1000];
        let err = stream_decode(data, 37).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(what) if what == "Grundig header"));
        assert!(matches!(
            crate::decode_to_buffer(data),
            Err(DecodeError::Truncated(what)) if what == "Grundig header"
        ));
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
    fn test_streaming_decoder_two_block_header_detects() {
        let mut data = vec![0u8; 1024 + 512];
        data[..4].copy_from_slice(b"\x02ds2");
        data[1024 + 2] = 1;
        data[1024 + 3] = 0xff;
        data[1024 + 4] = 6;
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..1024 + 4]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[1024 + 4..]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        let tail = decoder.finish().unwrap();
        assert_eq!(tail.len(), 256);
    }

    #[test]
    fn test_streaming_decoder_one_block_header_for_first_byte_one() {
        let mut data = vec![0u8; 512 + 512];
        data[..4].copy_from_slice(b"\x01ds2");
        data[512 + 2] = 1;
        data[512 + 3] = 0xff;
        data[512 + 4] = 6;
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x203]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x203..]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        let tail = decoder.finish().unwrap();
        assert_eq!(tail.len(), 256);
    }

    #[test]
    fn test_streaming_decoder_truncated_long_header_on_finish() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&[0x08, b'd', b's', b'2', 0, 0]).unwrap();
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&[0x11, b'd', b's', b'2', 0, 0]).unwrap();
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_streaming_decoder_rejects_header_byte_out_of_range() {
        let mut decoder = StreamingDecoder::new();
        let err = decoder.push(b"\x00ds2").unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedFormat(0)));
    }

    #[test]
    fn decrypt_streamer_passes_long_header_ds2_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x08ds2rest").unwrap();
        assert_eq!(plain, b"\x08ds2rest");
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
        // The fixture declares one frame in its block header, so finish()
        // decodes it (256 samples per QP frame)
        let tail = decoder.finish().unwrap();
        assert!(!tail.is_empty());
        assert_eq!(tail.len() % 256, 0);
    }

    #[test]
    fn decrypting_decoder_streamer_push_after_finish_errors() {
        let mut decoder = DecryptingDecoderStreamer::new(None);
        let _ = decoder.finish().unwrap();
        let err = decoder.push(b"\x03ds2").unwrap_err();
        assert!(matches!(err, DecodeError::AlreadyFinished));
    }

    fn joined(prefix: &[u8; 4], header_size: usize, blocks: &[(u8, u8)]) -> Vec<u8> {
        let mut data = vec![0u8; header_size];
        data[..4].copy_from_slice(prefix);
        for &(format, frame_count) in blocks {
            let mut block = sp_block(frame_count);
            block[4] = format;
            data.extend_from_slice(&block);
        }
        data
    }

    fn decode_parts(data: &[u8], chunk: usize) -> (Vec<f64>, Vec<AudioPart>) {
        let mut decoder = StreamingDecoder::new();
        let mut samples = Vec::new();
        for piece in data.chunks(chunk) {
            samples.extend(decoder.push(piece).unwrap());
        }
        samples.extend(decoder.finish().unwrap());
        (samples, decoder.parts().to_vec())
    }

    fn part(start: usize, len: usize, format: AudioFormat, block_format: u8) -> AudioPart {
        AudioPart {
            start,
            len,
            rate: format.native_sample_rate(),
            format,
            block_format,
        }
    }

    #[test]
    fn test_streaming_decoder_decodes_each_part_with_its_own_codec() {
        use AudioFormat::{Ds2Qp, Ds2Qp7, Ds2Sp, DssSp};
        for (prefix, header_size, first, second, formats) in [
            (b"\x02dss", 0x400, 0u8, 1u8, [DssSp, DssSp]),
            (b"\x02dss", 0x400, 1, 0, [DssSp, DssSp]),
            (b"\x03ds2", 0x600, 0, 6, [Ds2Sp, Ds2Qp]),
            (b"\x03ds2", 0x600, 1, 7, [Ds2Sp, Ds2Qp7]),
            (b"\x03ds2", 0x600, 6, 1, [Ds2Qp, Ds2Sp]),
            (b"\x03ds2", 0x600, 7, 0, [Ds2Qp7, Ds2Sp]),
            (b"\x03ds2", 0x600, 6, 7, [Ds2Qp, Ds2Qp7]),
            (b"\x03ds2", 0x600, 7, 6, [Ds2Qp7, Ds2Qp]),
        ] {
            let alone = |format| decode_parts(&joined(prefix, header_size, &[(format, 3)]), 100).0;
            let (head, tail) = (alone(first), alone(second));
            assert!(!head.is_empty() && !tail.is_empty());
            assert_ne!(head, tail);
            let expected = [head.clone(), tail.clone()].concat();
            let expected_parts = [
                part(0, head.len(), formats[0], first),
                part(head.len(), tail.len(), formats[1], second),
            ];
            let data = joined(prefix, header_size, &[(first, 3), (second, 3)]);
            for chunk in [1, 7, 100, data.len()] {
                let (samples, parts) = decode_parts(&data, chunk);
                assert_eq!(parts, expected_parts, "{first} then {second}, chunk {chunk}");
                assert_eq!(samples, expected, "{first} then {second}, chunk {chunk}");
            }
            let buffer = crate::decode_to_buffer(&data).unwrap();
            assert_eq!(buffer.format, formats[0]);
            assert_eq!(buffer.native_rate, formats[0].native_sample_rate());
            assert_eq!(buffer.parts, expected_parts);
            assert_eq!(buffer.samples, expected);
        }
    }

    #[test]
    fn test_unknown_block_format_takes_the_last_format_with_frames() {
        for (blocks, last) in [
            (&[(6u8, 3u8), (7, 3), (7, 3)][..], 2),
            (&[(7, 3), (6, 3), (6, 3)][..], 2),
            (&[(1, 3), (7, 3), (7, 3)][..], 2),
            (&[(6, 3), (0, 3), (0, 3)][..], 2),
            (&[(6, 3), (7, 3), (7, 0), (7, 3)][..], 3),
        ] {
            let expected = decode_parts(&joined(b"\x03ds2", 0x600, blocks), 100);
            for unknown in [3u8, 5, 8, 0xff] {
                let mut changed = blocks.to_vec();
                changed[last].0 = unknown;
                let data = joined(b"\x03ds2", 0x600, &changed);
                for chunk in [7, data.len()] {
                    assert_eq!(
                        decode_parts(&data, chunk),
                        expected,
                        "{blocks:?} with {unknown} in block {last}, chunk {chunk}"
                    );
                }
                let buffer = crate::decode_to_buffer(&data).unwrap();
                assert_eq!((buffer.samples, buffer.parts), expected);
            }
        }
    }

    #[test]
    fn test_unknown_format_before_any_known_takes_the_first_known() {
        for (prefix, header_size, blocks) in [
            (b"\x03ds2", 0x600, &[(6u8, 3u8), (6, 3)][..]),
            (b"\x03ds2", 0x600, &[(7, 3), (7, 3)][..]),
            (b"\x03ds2", 0x600, &[(0, 3), (0, 3)][..]),
            (b"\x03ds2", 0x600, &[(1, 3), (1, 3)][..]),
            (b"\x03ds2", 0x600, &[(7, 3), (6, 3)][..]),
            (b"\x03ds2", 0x600, &[(0, 3), (7, 3)][..]),
            (b"\x03ds2", 0x600, &[(7, 3), (0, 3)][..]),
            (b"\x02dss", 0x400, &[(0, 3), (0, 3)][..]),
            (b"\x02dss", 0x400, &[(1, 3), (1, 3)][..]),
        ] {
            let mut known = vec![(blocks[0].0, 3u8), (blocks[0].0, 3)];
            known.extend_from_slice(blocks);
            let expected = decode_parts(&joined(prefix, header_size, &known), 100);
            for unknown in [3u8, 5, 8, 0xff] {
                let mut changed = known.clone();
                changed[0].0 = unknown;
                changed[1].0 = unknown;
                let data = joined(prefix, header_size, &changed);
                for chunk in [7, data.len()] {
                    assert_eq!(
                        decode_parts(&data, chunk),
                        expected,
                        "{blocks:?} after {unknown}, chunk {chunk}"
                    );
                }
                let buffer = crate::decode_to_buffer(&data).unwrap();
                assert_eq!((buffer.samples, buffer.parts), expected);
            }
        }
        let unknown_only = joined(b"\x03ds2", 0x600, &[(3, 3), (3, 3)]);
        assert!(matches!(
            crate::decode_to_buffer(&unknown_only),
            Err(DecodeError::UnsupportedCodec(3))
        ));
    }

    #[test]
    fn test_file_without_a_qualifying_block_is_refused() {
        for (prefix, header_size, format) in [
            (b"\x03ds2", 0x600, 6u8),
            (b"\x03ds2", 0x600, 7),
            (b"\x03ds2", 0x600, 1),
            (b"\x02dss", 0x400, 0),
            (b"\x02dss", 0x400, 1),
            (b"\x06dss", 0xc00, 0),
        ] {
            let plain = joined(prefix, header_size, &[(format, 3), (format, 3), (3, 3)]);
            assert!(!crate::decode_to_buffer(&plain).unwrap().samples.is_empty());
            let mut data = plain.clone();
            for block in 0..2 {
                data[header_size + block * 512 + 4] = 5;
            }
            for chunk in [7, data.len()] {
                assert!(
                    matches!(
                        stream_decode(&data, chunk),
                        Err(DecodeError::UnsupportedCodec(5))
                    ),
                    "{prefix:?} format {format}, chunk {chunk}"
                );
            }
            assert!(matches!(
                crate::decode_to_buffer(&data),
                Err(DecodeError::UnsupportedCodec(5))
            ));
            let demuxed = if prefix[1..] == *b"ds2" {
                demux_ds2(&data).map(|_| ())
            } else {
                crate::demux::dss::demux_dss(&data).map(|_| ())
            };
            assert!(matches!(demuxed, Err(DecodeError::UnsupportedCodec(5))));

            let mut unset = plain.clone();
            for block in 0..2 {
                unset[header_size + block * 512 + 3] = 0;
            }
            assert_eq!(
                crate::decode_to_buffer(&unset).unwrap().samples,
                crate::decode_to_buffer(&plain).unwrap().samples,
                "{prefix:?} format {format}, byte 3 not 0xff"
            );
        }
    }

    #[test]
    fn test_block_without_byte_3_ff_does_not_start_a_part() {
        let ds2_frames = |data: &[u8]| match demux_ds2(data).unwrap() {
            DemuxedDs2::Sp { packets, .. } => (0, packets),
            DemuxedDs2::QpSegments { segments, .. } => (
                6,
                segments.into_iter().map(|segment| segment.stream).collect(),
            ),
            DemuxedDs2::Qp7Segments { segments, .. } => (
                7,
                segments
                    .into_iter()
                    .flat_map(|segment| segment.records)
                    .collect(),
            ),
        };
        for (prefix, header_size, format, other, at) in [
            (b"\x03ds2", 0x600, 6u8, 7u8, 1usize),
            (b"\x03ds2", 0x600, 7, 6, 1),
            (b"\x03ds2", 0x600, 1, 6, 1),
            (b"\x03ds2", 0x600, 6, 0, 1),
            (b"\x03ds2", 0x600, 6, 7, 0),
            (b"\x02dss", 0x400, 0, 1, 1),
            (b"\x02dss", 0x400, 1, 0, 1),
            (b"\x06dss", 0xc00, 0, 1, 1),
            (b"\x06dss", 0xc00, 0, 6, 1),
        ] {
            let plain = joined(prefix, header_size, &[(format, 3); 3]);
            let mut blocks = [(format, 3u8); 3];
            blocks[at].0 = other;
            let mut data = joined(prefix, header_size, &blocks);
            data[header_size + at * 512 + 3] = 0;
            let expected = decode_parts(&plain, 100);
            for chunk in [7, data.len()] {
                assert_eq!(
                    decode_parts(&data, chunk),
                    expected,
                    "{prefix:?} format {format} with {other} in block {at}, chunk {chunk}"
                );
            }
            let buffer = crate::decode_to_buffer(&data).unwrap();
            assert_eq!((buffer.samples, buffer.parts), expected);
            if prefix[1..] == *b"ds2" {
                assert_eq!(ds2_frames(&data), ds2_frames(&plain));
            } else {
                assert_eq!(
                    crate::demux::dss::demux_dss(&data).unwrap(),
                    crate::demux::dss::demux_dss(&plain).unwrap()
                );
            }
            if prefix[0] == 6 {
                assert_eq!(
                    crate::demux::grundig::demux_grundig(&data[header_size..]).unwrap(),
                    crate::demux::grundig::demux_grundig(&plain[header_size..]).unwrap()
                );
            }
        }
    }

    #[test]
    fn test_grundig_file_decodes_each_part_with_its_own_codec() {
        use AudioFormat::{Ds2Qp, Ds2Qp7, DssSp, GrundigSp};
        let head = joined(b"\x06dss", 0xc00, &[(0, 3), (0, 3)]);
        let (head_samples, head_parts) = decode_parts(&head, 100);
        assert_eq!(head_parts, [part(0, head_samples.len(), GrundigSp, 0)]);
        for (prefix, header_size, second, format) in [
            (b"\x02dss", 0x400, 1u8, DssSp),
            (b"\x03ds2", 0x600, 6, Ds2Qp),
            (b"\x03ds2", 0x600, 7, Ds2Qp7),
        ] {
            let alone = joined(prefix, header_size, &[(second, 3), (second, 3)]);
            let (tail_samples, _) = decode_parts(&alone, 100);
            let mut data = head.clone();
            data.extend_from_slice(&alone[header_size..]);
            let expected = (
                [head_samples.clone(), tail_samples.clone()].concat(),
                vec![
                    part(0, head_samples.len(), GrundigSp, 0),
                    part(head_samples.len(), tail_samples.len(), format, second),
                ],
            );
            for chunk in [7, data.len()] {
                assert_eq!(decode_parts(&data, chunk), expected, "0 then {second}");
            }
            let buffer = crate::decode_to_buffer(&data).unwrap();
            assert_eq!(buffer.format, GrundigSp);
            assert_eq!((buffer.samples, buffer.parts), expected);
        }
        for lp in [2u8, 4] {
            let data = joined(b"\x06dss", 0xc00, &[(0, 3), (0, 3), (lp, 3)]);
            assert!(matches!(
                crate::decode_to_buffer(&data),
                Err(DecodeError::UnsupportedCodec(c)) if c == lp
            ));
        }
        for unknown in [3u8, 5, 8, 0xff] {
            let data = joined(b"\x06dss", 0xc00, &[(0, 3), (0, 3), (unknown, 3)]);
            let same = joined(b"\x06dss", 0xc00, &[(0, 3), (0, 3), (0, 3)]);
            assert_eq!(
                decode_parts(&data, 100),
                decode_parts(&same, 100),
                "{unknown}"
            );
        }
    }

    fn cut_first_frame(format: u8) -> Vec<u8> {
        let mut block = sp_block(1);
        block[1] = 253;
        block[4] = format;
        block
    }

    #[test]
    fn test_first_part_starts_at_the_first_frame() {
        use AudioFormat::{Ds2Qp, Ds2Sp, DssSp};
        for (prefix, header_size, first, rest, format) in [
            (b"\x03ds2", 0x600, 6u8, 0u8, Ds2Sp),
            (b"\x03ds2", 0x600, 7, 1, Ds2Sp),
            (b"\x03ds2", 0x600, 0, 6, Ds2Qp),
            (b"\x03ds2", 0x600, 6, 1, Ds2Sp),
            (b"\x02dss", 0x400, 0, 1, DssSp),
        ] {
            let alone = joined(prefix, header_size, &[(rest, 3), (rest, 3)]);
            let mut data = vec![0u8; header_size];
            data[..4].copy_from_slice(prefix);
            data.extend_from_slice(&cut_first_frame(first));
            data.extend_from_slice(&alone[header_size..]);
            let expected = decode_parts(&alone, 100);
            assert_eq!(expected.1.len(), 1);
            for chunk in [1, 7, data.len()] {
                let mut decoder = StreamingDecoder::new();
                let mut samples = Vec::new();
                for piece in data.chunks(chunk) {
                    samples.extend(decoder.push(piece).unwrap());
                }
                samples.extend(decoder.finish().unwrap());
                assert_eq!(
                    (samples, decoder.parts().to_vec()),
                    expected,
                    "{first} then {rest}"
                );
                assert_eq!(decoder.format(), Some(format));
                assert_eq!(decoder.native_rate(), Some(format.native_sample_rate()));
            }
            let buffer = crate::decode_to_buffer(&data).unwrap();
            assert_eq!(buffer.format, format);
            assert_eq!(buffer.native_rate, format.native_sample_rate());
            assert_eq!((buffer.samples, buffer.parts), expected);
            if prefix == b"\x03ds2" {
                let kind = |data: &[u8]| match demux_ds2(data).unwrap() {
                    DemuxedDs2::Sp { packets, .. } => (0, packets),
                    DemuxedDs2::QpSegments { segments, .. } => {
                        (6, segments.into_iter().map(|seg| seg.stream).collect())
                    }
                    DemuxedDs2::Qp7Segments { segments, .. } => (
                        7,
                        segments.into_iter().flat_map(|seg| seg.records).collect(),
                    ),
                };
                let one_part = joined(prefix, header_size, &[(rest, 3)]);
                let mut cut = vec![0u8; header_size];
                cut[..4].copy_from_slice(prefix);
                cut.extend_from_slice(&cut_first_frame(first));
                cut.extend_from_slice(&one_part[header_size..]);
                assert_eq!(kind(&cut), kind(&one_part), "demux {first} then {rest}");
            }
        }
        let mut dss = vec![0u8; 0x400];
        dss[..4].copy_from_slice(b"\x02dss");
        dss.extend_from_slice(&cut_first_frame(0));
        dss.extend_from_slice(&joined(b"\x02dss", 0x400, &[(6, 3)])[0x400..]);
        assert!(matches!(
            crate::demux::dss::demux_dss(&dss),
            Err(DecodeError::UnsupportedCodec(6))
        ));
    }

    #[test]
    fn test_strict_finish_fails_on_a_cut_qp_part() {
        for blocks in [&[(1u8, 3u8), (6, 8)][..], &[(6, 3), (1, 3), (6, 8)][..]] {
            let data = joined(b"\x03ds2", 0x600, blocks);
            let last = data.len() - 512 + 6;
            for (cut, fails) in [
                (last + 3 * 56, true),
                (last + 3 * 56 + 20, true),
                (last + 8 * 56 - 1, true),
                (last + 8 * 56, false),
                (data.len(), false),
            ] {
                let mut decoder = StreamingDecoder::new();
                let mut samples = decoder.push(&data[..cut]).unwrap();
                match decoder.finish() {
                    Err(DecodeError::Truncated(what)) => {
                        assert!(fails, "{blocks:?} cut at {cut}");
                        assert_eq!(what, "DS2 QP frame");
                    }
                    Ok(tail) => {
                        assert!(!fails, "{blocks:?} cut at {cut}");
                        samples.extend(tail);
                    }
                    Err(other) => panic!("{blocks:?} cut at {cut}: {other}"),
                }
                let lenient = crate::decode_to_buffer(&data[..cut]).unwrap();
                if !fails {
                    assert_eq!(samples, lenient.samples);
                }
            }
        }
    }

    #[test]
    fn test_streaming_decoder_single_format_file_is_one_part() {
        use AudioFormat::{Ds2Qp, Ds2Qp7, Ds2Sp, DssSp, GrundigSp};
        for (data, format, block_format) in [
            (joined(b"\x02dss", 0x400, &[(0, 3), (0, 2)]), DssSp, 0),
            (joined(b"\x02dss", 0x400, &[(1, 3), (1, 2)]), DssSp, 1),
            (dss_with_header_blocks(6, 0), GrundigSp, 0),
            (joined(b"\x03ds2", 0x600, &[(1, 3), (1, 2)]), Ds2Sp, 1),
            (joined(b"\x03ds2", 0x600, &[(6, 3), (6, 2)]), Ds2Qp, 6),
            (joined(b"\x03ds2", 0x600, &[(7, 3), (7, 2)]), Ds2Qp7, 7),
        ] {
            let (samples, parts) = decode_parts(&data, 7);
            assert!(!samples.is_empty());
            assert_eq!(parts, [part(0, samples.len(), format, block_format)]);
            assert_eq!(crate::decode_to_buffer(&data).unwrap().parts, parts);
        }
        let empty = joined(b"\x02dss", 0x400, &[(0, 0)]);
        assert_eq!(decode_parts(&empty, 7), (Vec::new(), Vec::new()));
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_part() {
        for (prefix, header_size, first) in
            [(b"\x02dss", 0x400, 0u8), (b"\x03ds2", 0x600, 1), (b"\x03ds2", 0x600, 6)]
        {
            let (head, head_parts) = decode_parts(&joined(prefix, header_size, &[(first, 3)]), 100);
            for lp in [2u8, 4] {
                let data = joined(prefix, header_size, &[(first, 3), (lp, 3)]);
                for chunk in [1, 7, data.len()] {
                    let mut decoder = StreamingDecoder::new();
                    let mut samples = Vec::new();
                    let mut err = None;
                    for piece in data.chunks(chunk) {
                        match decoder.push(piece) {
                            Ok(more) => samples.extend(more),
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                    let err = err.or_else(|| decoder.finish().err());
                    assert!(matches!(err, Some(DecodeError::UnsupportedCodec(c)) if c == lp));
                    if first == 6 {
                        assert!(samples.is_empty());
                    } else {
                        assert_eq!(samples, head);
                        assert_eq!(decoder.parts(), head_parts);
                    }
                }
                assert!(matches!(
                    crate::decode_to_buffer(&data),
                    Err(DecodeError::UnsupportedCodec(c)) if c == lp
                ));
            }
        }
    }

    #[test]
    fn test_demux_refuses_more_than_one_part() {
        let frames = |data: &[u8]| match demux_ds2(data).unwrap() {
            DemuxedDs2::Sp { total_frames, .. } => (0, total_frames),
            DemuxedDs2::QpSegments { total_frames, .. } => (6, total_frames),
            DemuxedDs2::Qp7Segments { total_frames, .. } => (7, total_frames),
        };
        for (first, second, kind) in [
            (1u8, 6u8, 0),
            (1, 4, 0),
            (6, 0, 6),
            (6, 2, 6),
            (6, 7, 6),
            (7, 6, 7),
        ] {
            let data = joined(b"\x03ds2", 0x600, &[(first, 3), (second, 4)]);
            assert!(
                matches!(demux_ds2(&data), Err(DecodeError::MultipleParts(f)) if f == second),
                "{first} then {second}"
            );
            let data = joined(b"\x03ds2", 0x600, &[(first, 3), (first, 4)]);
            assert_eq!(frames(&data), (kind, 7), "{first} alone");
        }
        for second in [1u8, 2] {
            let dss = joined(b"\x02dss", 0x400, &[(0, 3), (second, 4)]);
            assert!(
                matches!(
                    crate::demux::dss::demux_dss(&dss),
                    Err(DecodeError::MultipleParts(f)) if f == second
                ),
                "dss 0 then {second}"
            );
        }
        let dss = joined(b"\x02dss", 0x400, &[(0, 3), (0, 4)]);
        let (packets, total) = crate::demux::dss::demux_dss(&dss).unwrap();
        assert_eq!((packets.len(), total), (7, 7));
    }
}
