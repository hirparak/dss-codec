use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a DSS file: {0}")]
    NotDss(PathBuf),

    #[error("not a DS2 file: {0}")]
    NotDs2(PathBuf),

    #[error("unsupported DS2 format type: {0}")]
    UnsupportedFormat(u8),

    #[error("encrypted DS2 error: {0}")]
    EncryptedDs2(String),

    #[error("bitstream exhausted: needed {needed} bits, {available} available")]
    BitstreamExhausted { needed: usize, available: usize },

    #[error("invalid frame data at frame {frame}: {detail}")]
    InvalidFrame { frame: usize, detail: String },

    #[error("truncated data: {0}")]
    Truncated(String),

    #[error("streaming decoder is already finished")]
    AlreadyFinished,

    #[error("WAV write error: {0}")]
    Wav(#[from] hound::Error),

    #[error("resample error: {0}")]
    Resample(String),
}

pub type Result<T> = std::result::Result<T, DecodeError>;
