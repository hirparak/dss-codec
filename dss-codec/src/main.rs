use clap::Parser;
use dss_codec::crypto::ds2_encrypted::{parse_decrypt_descriptor, ENCRYPTED_MAGIC};
use dss_codec::demux::detect_format;
use dss_codec::output::OutputConfig;
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
    let password = resolve_password(cli.password.as_deref());

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
                        let duration = buf.samples.len() as f64 / buf.native_rate as f64;
                        eprintln!(
                            "  {} -> {} ({:.1}s, {} Hz, {:?})",
                            input_path.display(),
                            output_path.display(),
                            duration,
                            buf.native_rate,
                            buf.format,
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

fn resolve_password(cli_password: Option<&str>) -> Option<Vec<u8>> {
    cli_password
        .map(str::as_bytes)
        .map(|bytes| bytes.to_vec())
        .or_else(|| env::var("DSS_CODEC_PASSWORD").ok().map(|value| value.into_bytes()))
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
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error reading {}: {}", path.display(), e);
            return;
        }
    };

    if data.starts_with(&ENCRYPTED_MAGIC) {
        match parse_decrypt_descriptor(&data) {
            Ok(desc) => {
                println!(
                    "{}: encrypted DS2 ({:?}), password required",
                    path.display(),
                    desc.key_mode
                );
            }
            Err(e) => {
                println!("{}: encrypted DS2 (descriptor error: {})", path.display(), e);
            }
        }
        return;
    }

    match detect_format(&data) {
        Some(fmt) => {
            println!(
                "{}: {:?}, native rate {} Hz",
                path.display(),
                fmt,
                fmt.native_sample_rate()
            );
        }
        None => {
            println!("{}: unknown format", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{make_decrypt_output_path, resolve_password};
    use std::env;
    use std::path::PathBuf;

    #[test]
    fn resolve_password_prefers_cli_over_env() {
        unsafe {
            env::set_var("DSS_CODEC_PASSWORD", "env-secret");
        }
        assert_eq!(resolve_password(Some("cli-secret")), Some(b"cli-secret".to_vec()));
        unsafe {
            env::remove_var("DSS_CODEC_PASSWORD");
        }
    }

    #[test]
    fn resolve_password_falls_back_to_env() {
        unsafe {
            env::set_var("DSS_CODEC_PASSWORD", "env-secret");
        }
        assert_eq!(resolve_password(None), Some(b"env-secret".to_vec()));
        unsafe {
            env::remove_var("DSS_CODEC_PASSWORD");
        }
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
