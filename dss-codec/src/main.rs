use clap::Parser;
use dss_codec::output::OutputConfig;
use dss_codec::EncryptionInfo;
use std::env;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "dss-decode", about = "Decode DSS/DS2 audio files")]
struct Cli {
    /// Input file(s)
    #[arg(required = true)]
    input: Vec<PathBuf>,

    /// Output file (single input) or ignored (batch mode)
    #[arg(short = 'O', long)]
    output_file: Option<PathBuf>,

    /// Output format
    #[arg(short = 'f', long, default_value = "wav")]
    format: String,

    /// Output sample rate (default: native)
    #[arg(short = 'r', long)]
    rate: Option<u32>,

    /// Bit depth
    #[arg(short = 'b', long, default_value = "16")]
    bits: u16,

    /// Channels (1=mono, 2=stereo)
    #[arg(short = 'c', long, default_value = "1")]
    channels: u16,

    /// Batch output directory
    #[arg(short = 'o', long)]
    output_dir: Option<PathBuf>,

    /// Suppress output
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Print file metadata only
    #[arg(long)]
    info: bool,

    /// Save decrypted/plain container bytes instead of decoding to WAV
    #[arg(long)]
    decrypt: bool,

    /// Password for encrypted DS2 input (or set DSS_CODEC_PASSWORD)
    #[arg(long)]
    password: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    if cli.info {
        for path in &cli.input {
            print_info(path, cli.quiet);
        }
        return;
    }

    let config = OutputConfig {
        sample_rate: cli.rate,
        bit_depth: cli.bits,
        channels: cli.channels,
    };
    let password = resolve_password(cli.password.as_deref(), env::var("DSS_CODEC_PASSWORD").ok());

    for input_path in &cli.input {
        let output_path = if cli.decrypt {
            if let Some(ref out) = cli.output_file {
                if cli.input.len() == 1 {
                    out.clone()
                } else {
                    make_decrypt_output_path(input_path, cli.output_dir.as_deref())
                }
            } else {
                make_decrypt_output_path(input_path, cli.output_dir.as_deref())
            }
        } else if let Some(ref out) = cli.output_file {
            if cli.input.len() == 1 {
                out.clone()
            } else {
                make_output_path(input_path, cli.output_dir.as_deref(), &cli.format)
            }
        } else {
            make_output_path(input_path, cli.output_dir.as_deref(), &cli.format)
        };

        if !cli.quiet {
            eprintln!(
                "{}: {}",
                if cli.decrypt { "Decrypting" } else { "Decoding" },
                input_path.display()
            );
        }

        if cli.decrypt {
            match dss_codec::decrypt_file(input_path, password.as_deref()) {
                Ok(bytes) => {
                    if let Err(e) = std::fs::write(&output_path, &bytes) {
                        eprintln!("Error writing {}: {}", output_path.display(), e);
                        std::process::exit(1);
                    }
                    if !cli.quiet {
                        eprintln!(
                            "  {} -> {} ({} bytes)",
                            input_path.display(),
                            output_path.display(),
                            bytes.len(),
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error decrypting {}: {}", input_path.display(), e);
                    std::process::exit(1);
                }
            }
        } else {
            match dss_codec::decode_and_write_with_password(
                input_path,
                &output_path,
                &config,
                password.as_deref(),
            ) {
                Ok(buf) => {
                    if !cli.quiet {
                        eprintln!(
                            "{}",
                            summary(input_path, &output_path, &buf, config.sample_rate)
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error decoding {}: {}", input_path.display(), e);
                    std::process::exit(1);
                }
            }
        }
    }
}

fn resolve_password(cli_password: Option<&str>, env_password: Option<String>) -> Option<Vec<u8>> {
    cli_password
        .map(str::as_bytes)
        .map(|bytes| bytes.to_vec())
        .or_else(|| env_password.map(|value| value.into_bytes()))
}

fn summary(
    input: &std::path::Path,
    output: &std::path::Path,
    buf: &dss_codec::AudioBuffer,
    rate: Option<u32>,
) -> String {
    let rate = rate.unwrap_or(buf.native_rate);
    let duration = buf.samples.len() as f64 / rate as f64;
    let formats = if buf.parts.len() > 1 {
        let formats: Vec<String> = buf
            .parts
            .iter()
            .map(|part| format!("{:?}", part.format))
            .collect();
        formats.join(" then ")
    } else {
        format!("{:?}", buf.format)
    };
    format!(
        "  {} -> {} ({:.1}s, {} Hz, {})",
        input.display(),
        output.display(),
        duration,
        rate,
        formats,
    )
}

fn make_output_path(input: &PathBuf, output_dir: Option<&std::path::Path>, ext: &str) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let filename = format!("{}.{}", stem.to_string_lossy(), ext);
    if let Some(dir) = output_dir {
        dir.join(filename)
    } else {
        input.with_file_name(filename)
    }
}

fn make_decrypt_output_path(input: &PathBuf, output_dir: Option<&std::path::Path>) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let ext = input.extension().and_then(|ext| ext.to_str()).unwrap_or("bin");
    let filename = format!("{}.decrypted.{}", stem.to_string_lossy(), ext);
    if let Some(dir) = output_dir {
        dir.join(filename)
    } else {
        input.with_file_name(filename)
    }
}

fn print_info(path: &PathBuf, _quiet: bool) {
    match dss_codec::inspect_file(path) {
        Ok(info) => match info.encryption {
            EncryptionInfo::None => println!(
                "{}: {:?}, native rate {} Hz",
                path.display(),
                info.format,
                info.native_rate()
            ),
            EncryptionInfo::EncryptedDs2Aes128 => println!(
                "{}: encrypted DS2 (AES-128), native rate {} Hz, password required",
                path.display(),
                info.native_rate()
            ),
            EncryptionInfo::EncryptedDs2Aes256 => println!(
                "{}: encrypted DS2 (AES-256), native rate {} Hz, password required",
                path.display(),
                info.native_rate()
            ),
            EncryptionInfo::EncryptedUnknown(mode) => println!(
                "{}: encrypted container (mode 0x{:04x}), native rate {} Hz",
                path.display(),
                mode,
                info.native_rate()
            ),
        },
        Err(dss_codec::error::DecodeError::Io(e)) => {
            eprintln!("Error reading {}: {}", path.display(), e);
        }
        Err(e) => println!("{}: info unavailable ({})", path.display(), e),
    }
}

#[cfg(test)]
mod tests {
    use super::{make_decrypt_output_path, resolve_password, summary};
    use dss_codec::demux::AudioFormat;
    use dss_codec::{AudioBuffer, AudioPart};
    use std::path::{Path, PathBuf};

    #[test]
    fn resolve_password_prefers_cli_over_env() {
        assert_eq!(
            resolve_password(Some("cli-secret"), Some("env-secret".to_string())),
            Some(b"cli-secret".to_vec())
        );
    }

    #[test]
    fn resolve_password_falls_back_to_env() {
        assert_eq!(
            resolve_password(None, Some("env-secret".to_string())),
            Some(b"env-secret".to_vec())
        );
    }

    #[test]
    fn resolve_password_is_none_without_cli_or_env() {
        assert_eq!(resolve_password(None, None), None);
    }

    #[test]
    fn summary_gives_the_output_rate_and_duration() {
        let buf = AudioBuffer {
            samples: vec![0.0; 288000],
            native_rate: 16000,
            format: AudioFormat::Ds2Qp,
            parts: Vec::new(),
        };
        let input = Path::new("in.ds2");
        let output = Path::new("out.wav");
        assert_eq!(
            summary(input, output, &buf, Some(12000)),
            "  in.ds2 -> out.wav (24.0s, 12000 Hz, Ds2Qp)"
        );
        let buf = AudioBuffer {
            samples: vec![0.0; 384000],
            ..buf
        };
        assert_eq!(
            summary(input, output, &buf, None),
            "  in.ds2 -> out.wav (24.0s, 16000 Hz, Ds2Qp)"
        );
    }

    #[test]
    fn summary_names_the_format_of_every_part() {
        let part = |start, format: AudioFormat| AudioPart {
            start,
            len: 384000,
            rate: 16000,
            format,
            block_format: 0,
        };
        let buf = AudioBuffer {
            samples: vec![0.0; 1152000],
            native_rate: 12000,
            format: AudioFormat::Ds2Sp,
            parts: vec![
                part(0, AudioFormat::Ds2Sp),
                part(384000, AudioFormat::Ds2Qp),
                part(768000, AudioFormat::Ds2Sp),
            ],
        };
        assert_eq!(
            summary(Path::new("in.ds2"), Path::new("out.wav"), &buf, Some(16000)),
            "  in.ds2 -> out.wav (72.0s, 16000 Hz, Ds2Sp then Ds2Qp then Ds2Sp)"
        );
        let buf = AudioBuffer {
            parts: buf.parts[..1].to_vec(),
            ..buf
        };
        assert_eq!(
            summary(Path::new("in.ds2"), Path::new("out.wav"), &buf, Some(16000)),
            "  in.ds2 -> out.wav (72.0s, 16000 Hz, Ds2Sp)"
        );
    }

    #[test]
    fn make_decrypt_output_path_appends_decrypted_suffix() {
        let input = PathBuf::from("sample.ds2");
        assert_eq!(
            make_decrypt_output_path(&input, None),
            PathBuf::from("sample.decrypted.ds2")
        );
    }
}
