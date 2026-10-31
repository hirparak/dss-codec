use std::path::Path;

#[test]
fn test_detect_ds2_sp() {
    let path = Path::new("../test_files/sample_sp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place an SP-mode .DS2 file at test_files/sample_sp.DS2)");
        return;
    }
    let data = std::fs::read(path).unwrap();
    let fmt = dss_codec::demux::detect_format(&data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::Ds2Sp);
}

#[test]
fn test_detect_ds2_qp() {
    let path = Path::new("../test_files/sample_qp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place a QP-mode .DS2 file at test_files/sample_qp.DS2)");
        return;
    }
    let data = std::fs::read(path).unwrap();
    let fmt = dss_codec::demux::detect_format(&data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::Ds2Qp);
}

#[test]
fn test_decode_ds2_sp_basic() {
    let path = Path::new("../test_files/sample_sp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 12000);
    assert!(!buf.samples.is_empty());
    // SP mode: 288 samples per frame
    assert_eq!(buf.samples.len() % 288, 0);
}

#[test]
fn test_decode_ds2_qp_basic() {
    let path = Path::new("../test_files/sample_qp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 16000);
    assert!(!buf.samples.is_empty());
    // QP mode: 256 samples per frame
    assert_eq!(buf.samples.len() % 256, 0);
}

#[test]
fn test_decode_dss_sp_basic() {
    let path = Path::new("../test_files/sample.DSS");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place a .DSS file at test_files/sample.DSS)");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 12000);
    assert!(!buf.samples.is_empty());
}

#[test]
fn test_detect_grundig_sp() {
    let data = include_bytes!("fixtures/grundig_sample.dss");
    let fmt = dss_codec::demux::detect_format(data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::GrundigSp);
    assert_eq!(fmt.native_sample_rate(), 12000);
}

/// The 3:4 polyphase playback FIR from 12 kHz to 16 kHz: 100 taps per phase,
/// one extra tap on phase 0, and 0xce zero samples of history.
fn playback_fir(inp: &[f64]) -> Vec<i16> {
    use dss_codec::tables::grundig_sp::RESAMP;
    const HIST: usize = 0xce;
    let mut x = vec![0.0f64; HIST];
    x.extend_from_slice(inp);
    let n = x.len();
    let mut out = Vec::new();
    let mut uv: i64 = 0;
    let mut pos: usize = HIST;
    while pos < n {
        let mut acc = 0.0f64;
        let mut c = uv as usize;
        for k in 0..100 {
            acc += x[pos - k] * RESAMP[c];
            c += 4;
        }
        if uv == 0 {
            acc += x[pos - 100] * RESAMP[400];
        }
        out.push(if acc <= 32767.5 {
            if acc >= -32767.5 {
                (acc + 0.5).floor() as i16
            } else {
                -32767
            }
        } else {
            32767
        });
        uv += 3;
        if uv > 3 {
            let adv = uv >> 2;
            pos += adv as usize;
            uv -= 4 * adv;
        }
    }
    out
}

fn wav_samples(wav: &[u8]) -> Vec<i16> {
    wav[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[test]
fn test_decode_grundig_sp_bit_exact() {
    use std::io::Write;

    // Decode the committed Grundig .dss sample to a 12 kHz WAV, run the 3:4
    // playback FIR over the decode and compare every sample with the 16 kHz
    // reference, which Grundig DigtaSoft One wrote from the same .dss file.
    let data = include_bytes!("fixtures/grundig_sample.dss");
    let reference = include_bytes!("fixtures/grundig_sample_16k.wav");

    let dir = std::env::temp_dir();
    let in_path = dir.join("dss_codec_grundig_in.dss");
    let out_path = dir.join("dss_codec_grundig_out.wav");
    {
        let mut f = std::fs::File::create(&in_path).unwrap();
        f.write_all(data).unwrap();
    }

    let buf = dss_codec::decode_and_write(
        &in_path,
        &out_path,
        &dss_codec::output::OutputConfig::default(),
    )
    .unwrap();

    assert_eq!(buf.format, dss_codec::demux::AudioFormat::GrundigSp);
    assert_eq!(buf.native_rate, 12000);
    // 151 frames of 288 samples at 12 kHz.
    assert_eq!(buf.samples.len(), 151 * 288);

    let produced = std::fs::read(&out_path).unwrap();
    assert_eq!(u32::from_le_bytes(produced[24..28].try_into().unwrap()), 12000);
    let pcm12 = wav_samples(&produced);
    assert_eq!(pcm12.len(), buf.samples.len());
    let pcm12: Vec<f64> = pcm12.into_iter().map(f64::from).collect();
    assert_eq!(pcm12, buf.samples);

    let reference = wav_samples(reference);
    assert_eq!(reference.len(), 151 * 384);
    assert!(
        playback_fir(&pcm12) == reference,
        "decode through the playback FIR differs from the reference"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
}

#[test]
fn test_decode_and_write_resamples_each_part() {
    let mut data = vec![0u8; 0x600];
    data[..4].copy_from_slice(b"\x03ds2");
    for format in [0u8, 6] {
        let mut block = vec![0u8; 512];
        block[1] = 3;
        block[2] = 3;
        block[3] = 0xff;
        block[4] = format;
        for (i, byte) in block[6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        data.extend_from_slice(&block);
    }

    let dir = std::env::temp_dir();
    let in_path = dir.join("dss_codec_parts_in.ds2");
    let out_path = dir.join("dss_codec_parts_out.wav");
    std::fs::write(&in_path, &data).unwrap();

    let decoded = dss_codec::decode_file(&in_path).unwrap();
    assert_eq!(decoded.native_rate, 12000);
    assert_eq!(decoded.samples.len(), 3 * 288 + 3 * 256);

    for (rate, lens) in [
        (None, [3 * 288, 3 * 192]),
        (Some(12000), [3 * 288, 3 * 192]),
        (Some(16000), [3 * 384, 3 * 256]),
    ] {
        let config = dss_codec::output::OutputConfig {
            sample_rate: rate,
            ..Default::default()
        };
        let buf = dss_codec::decode_and_write(&in_path, &out_path, &config).unwrap();
        let rate = rate.unwrap_or(12000);
        let parts: Vec<_> = buf
            .parts
            .iter()
            .map(|part| (part.start, part.len, part.rate, part.block_format))
            .collect();
        assert_eq!(parts, [(0, lens[0], rate, 0), (lens[0], lens[1], rate, 6)]);
        assert_eq!(buf.samples.len(), lens[0] + lens[1]);

        let produced = std::fs::read(&out_path).unwrap();
        assert_eq!(u32::from_le_bytes(produced[24..28].try_into().unwrap()), rate);
        assert_eq!(wav_samples(&produced).len(), lens[0] + lens[1]);
        if rate == 12000 {
            assert_eq!(buf.samples[..lens[0]], decoded.samples[..lens[0]]);
        } else {
            assert_eq!(buf.samples[lens[0]..], decoded.samples[3 * 288..]);
        }
    }

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
}
