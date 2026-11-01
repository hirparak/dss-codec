use dss_codec::demux::AudioFormat;
use dss_codec::error::DecodeError;
use dss_codec::streaming::StreamingDecoder;
use dss_codec::AudioBuffer;

const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

fn md5(data: &[u8]) -> String {
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&(data.len() as u64 * 8).to_le_bytes());
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for (i, (&k, &s)) in K.iter().zip(S.iter()).enumerate() {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(s));
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(add);
        }
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/dssds2/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn decode(name: &str, password: Option<&[u8]>) -> dss_codec::error::Result<AudioBuffer> {
    dss_codec::decode_to_buffer_with_password(&fixture(name), password)
}

/// A file that lost its header decodes to the same samples as its original.
fn same_as_original(name: &str, original: &str) {
    let lost = decode(name, None).unwrap_or_else(|e| panic!("{name}: {e}"));
    let whole = decode(original, None).unwrap_or_else(|e| panic!("{original}: {e}"));
    assert_eq!(lost.native_rate, whole.native_rate, "{name}: rate");
    assert_eq!(lost.format, whole.format, "{name}: format");
    assert_eq!(pcm(&lost.samples), pcm(&whole.samples), "{name}: samples");
}

fn pcm(samples: &[f64]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| (sample.clamp(-32768.0, 32767.0) as i16).to_le_bytes())
        .collect()
}

fn check_parts(name: &str, buffer: &AudioBuffer, parts: &[(u32, usize, AudioFormat, u8)]) {
    let total: usize = parts.iter().map(|part| part.1).sum();
    assert_eq!(buffer.samples.len(), total, "{name}: samples");
    assert_eq!(buffer.native_rate, parts[0].0, "{name}: native rate");
    assert_eq!(buffer.format, parts[0].2, "{name}: format");
    assert_eq!(buffer.parts.len(), parts.len(), "{name}: parts");
    let mut start = 0;
    for (part, &(rate, len, format, block_format)) in buffer.parts.iter().zip(parts) {
        assert_eq!(
            (
                part.start,
                part.len,
                part.rate,
                part.format,
                part.block_format
            ),
            (start, len, rate, format, block_format),
            "{name}: part at {start}"
        );
        start += len;
    }
}

fn decodes(name: &str, password: Option<&[u8]>, parts: &[(u32, usize, AudioFormat, u8, &str)]) {
    let buffer = decode(name, password).unwrap_or_else(|e| panic!("{name}: {e}"));
    let layout: Vec<_> = parts.iter().map(|p| (p.0, p.1, p.2, p.3)).collect();
    check_parts(name, &buffer, &layout);
    for (part, expected) in buffer.parts.iter().zip(parts) {
        let samples = &buffer.samples[part.start..part.start + part.len];
        assert_eq!(
            md5(&pcm(samples)),
            expected.4,
            "{name}: md5 of the part at {}",
            part.start
        );
    }
}

fn refuses(name: &str, expected: fn(&DecodeError) -> bool) {
    match decode(name, None) {
        Ok(buffer) => panic!("{name}: decoded {} samples", buffer.samples.len()),
        Err(e) => assert!(expected(&e), "{name}: {e}"),
    }
}

/// The files with LP blocks decode to the same parts when they arrive in
/// pieces.
#[test]
fn lp_files_stream_like_a_whole_file() {
    for name in [
        "ds2/olympus-lp-annot.ds2",
        "ds2/olympus-lp-empty64.ds2",
        "ds2/olympus-lp-format4.ds2",
        "ds2/olympus-lp.ds2",
        "ds2/olympus-qp7-lp-empty109.ds2",
        "ds2/olympus-qp7-lp.ds2",
        "dss/olympus-lp-format-sp.dss",
        "dss/olympus-lp-format4.dss",
    ] {
        let data = fixture(name);
        let whole = decode(name, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        for chunk in [7, 515] {
            let mut decoder = StreamingDecoder::new();
            let mut samples = Vec::new();
            for piece in data.chunks(chunk) {
                samples.extend(decoder.push(piece).unwrap());
            }
            samples.extend(decoder.finish().unwrap());
            assert_eq!(samples, whole.samples, "{name}: samples, chunk {chunk}");
            assert_eq!(decoder.parts(), whole.parts, "{name}: parts, chunk {chunk}");
        }
    }
}

#[test]
fn md5_matches_rfc_1321() {
    for (text, digest) in [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ] {
        assert_eq!(md5(text.as_bytes()), digest, "{text:?}");
    }
}

#[test]
fn ds2_olympus_format1_annot() {
    decodes(
        "ds2/olympus-format1-annot.ds2",
        None,
        &[(
            12000,
            120096,
            AudioFormat::Ds2Sp,
            1,
            "8bee9594fca3c81b0e9c81dcbec5c8b4",
        )],
    );
}

#[test]
fn ds2_olympus_format1_header_copy() {
    decodes(
        "ds2/olympus-format1-header-copy.ds2",
        None,
        &[(
            12000,
            36000,
            AudioFormat::Ds2Sp,
            1,
            "29b4f756226dbdce7aa2450133098802",
        )],
    );
}

#[test]
fn ds2_olympus_format1() {
    decodes(
        "ds2/olympus-format1.ds2",
        None,
        &[(
            12000,
            120096,
            AudioFormat::Ds2Sp,
            1,
            "8bee9594fca3c81b0e9c81dcbec5c8b4",
        )],
    );
}

#[test]
fn ds2_olympus_lp_annot() {
    decodes(
        "ds2/olympus-lp-annot.ds2",
        None,
        &[(
            8000,
            80400,
            AudioFormat::Ds2Lp,
            2,
            "db6ef8b6e0ea569f10cf51a52f08cbc2",
        )],
    );
}

#[test]
fn ds2_olympus_lp_empty64() {
    decodes(
        "ds2/olympus-lp-empty64.ds2",
        None,
        &[(
            8000,
            80400,
            AudioFormat::Ds2Lp,
            2,
            "db6ef8b6e0ea569f10cf51a52f08cbc2",
        )],
    );
}

#[test]
fn ds2_olympus_lp_format4() {
    decodes(
        "ds2/olympus-lp-format4.ds2",
        None,
        &[(
            8000,
            24000,
            AudioFormat::Ds2Lp,
            4,
            "cb0ce2d909492c16a818557b1dcb29ba",
        )],
    );
}

#[test]
fn ds2_olympus_lp() {
    decodes(
        "ds2/olympus-lp.ds2",
        None,
        &[(
            8000,
            80400,
            AudioFormat::Ds2Lp,
            2,
            "db6ef8b6e0ea569f10cf51a52f08cbc2",
        )],
    );
}

#[test]
fn ds2_olympus_qp_aes_mode0() {
    decodes(
        "ds2/olympus-qp-aes-mode0.ds2",
        Some(b"test1234"),
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_aes256_mode0() {
    decodes(
        "ds2/olympus-qp-aes256-mode0.ds2",
        Some(b"test1234"),
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_annot() {
    decodes(
        "ds2/olympus-qp-annot.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_byte3_mid() {
    decodes(
        "ds2/olympus-qp-byte3-mid.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_byte3_none() {
    decodes(
        "ds2/olympus-qp-byte3-none.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_byte3_sp_first() {
    decodes(
        "ds2/olympus-qp-byte3-sp-first.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_count19() {
    decodes(
        "ds2/olympus-qp-count19.ds2",
        None,
        &[(
            16000,
            176896,
            AudioFormat::Ds2Qp,
            6,
            "759432a0076049d6de39b96df4bf5cf9",
        )],
    );
}

#[test]
fn ds2_olympus_qp_count19_byte3() {
    decodes(
        "ds2/olympus-qp-count19-byte3.ds2",
        None,
        &[(
            16000,
            176896,
            AudioFormat::Ds2Qp,
            6,
            "759432a0076049d6de39b96df4bf5cf9",
        )],
    );
}

#[test]
fn ds2_olympus_qp_cut() {
    decodes(
        "ds2/olympus-qp-cut.ds2",
        None,
        &[(
            16000,
            29184,
            AudioFormat::Ds2Qp,
            6,
            "fa2927815403e57e0e75bd65d03dcdfd",
        )],
    );
}

#[test]
fn ds2_olympus_qp_format3_first() {
    decodes(
        "ds2/olympus-qp-format3-first.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_hdr1() {
    decodes(
        "ds2/olympus-qp-hdr1.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_hdr17() {
    decodes(
        "ds2/olympus-qp-hdr17.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_header_copy() {
    decodes(
        "ds2/olympus-qp-header-copy.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_no_header() {
    same_as_original("ds2/olympus-qp-no-header.ds2", "ds2/olympus-qp-annot.ds2");
}

#[test]
fn ds2_olympus_qp_pause_block() {
    decodes(
        "ds2/olympus-qp-pause-block.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp,
            6,
            "c4d0fcd1e47d48b4ec8e85aed2fd7488",
        )],
    );
}

#[test]
fn ds2_olympus_qp_philips_pause() {
    decodes(
        "ds2/olympus-qp-philips-pause.ds2",
        None,
        &[(
            16000,
            178944,
            AudioFormat::Ds2Qp,
            6,
            "b4fe8e94e760538834711caf75e646ea",
        )],
    );
}

#[test]
fn ds2_olympus_qp_qp7_format3() {
    decodes(
        "ds2/olympus-qp-qp7-format3.ds2",
        None,
        &[
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp7,
                7,
                "205588ab2b9d57a3598e3a3c97d4a3f4",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_qp_qp7() {
    decodes(
        "ds2/olympus-qp-qp7.ds2",
        None,
        &[
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp7,
                7,
                "205588ab2b9d57a3598e3a3c97d4a3f4",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_qp_resync() {
    decodes(
        "ds2/olympus-qp-resync.ds2",
        None,
        &[(
            16000,
            47616,
            AudioFormat::Ds2Qp,
            6,
            "815b9f3dcd72602ebb65635d98a78c31",
        )],
    );
}

#[test]
fn ds2_olympus_qp_tail_start() {
    decodes(
        "ds2/olympus-qp-tail-start.ds2",
        None,
        &[(
            16000,
            34048,
            AudioFormat::Ds2Qp,
            6,
            "219532b42b2a87b74d5a78b4856a18e3",
        )],
    );
}

#[test]
fn ds2_olympus_qp7_empty_lp() {
    decodes(
        "ds2/olympus-qp7-empty-lp.ds2",
        None,
        &[(
            16000,
            48128,
            AudioFormat::Ds2Qp7,
            7,
            "205588ab2b9d57a3598e3a3c97d4a3f4",
        )],
    );
}

#[test]
fn ds2_olympus_qp7_empty64() {
    decodes(
        "ds2/olympus-qp7-empty64.ds2",
        None,
        &[(
            16000,
            160000,
            AudioFormat::Ds2Qp7,
            7,
            "07ddc7e3e7b61e7378e3209ca89b7986",
        )],
    );
}

#[test]
fn ds2_olympus_qp7_lp_empty109() {
    decodes(
        "ds2/olympus-qp7-lp-empty109.ds2",
        None,
        &[
            (
                16000,
                48128,
                AudioFormat::Ds2Qp7,
                7,
                "205588ab2b9d57a3598e3a3c97d4a3f4",
            ),
            (8000, 24000, AudioFormat::Ds2Lp, 2, "cb0ce2d909492c16a818557b1dcb29ba"),
        ],
    );
}

#[test]
fn ds2_olympus_qp7_lp() {
    decodes(
        "ds2/olympus-qp7-lp.ds2",
        None,
        &[
            (
                16000,
                48128,
                AudioFormat::Ds2Qp7,
                7,
                "205588ab2b9d57a3598e3a3c97d4a3f4",
            ),
            (8000, 24000, AudioFormat::Ds2Lp, 2, "cb0ce2d909492c16a818557b1dcb29ba"),
        ],
    );
}

#[test]
fn ds2_olympus_qp7() {
    decodes(
        "ds2/olympus-qp7.ds2",
        None,
        &[(
            16000,
            160000,
            AudioFormat::Ds2Qp7,
            7,
            "07ddc7e3e7b61e7378e3209ca89b7986",
        )],
    );
}

#[test]
fn ds2_olympus_sp_annot() {
    decodes(
        "ds2/olympus-sp-annot.ds2",
        None,
        &[(
            12000,
            120096,
            AudioFormat::Ds2Sp,
            0,
            "b42e989a4c162160557557e40b9362cb",
        )],
    );
}

#[test]
fn ds2_olympus_sp_empty_lp() {
    decodes(
        "ds2/olympus-sp-empty-lp.ds2",
        None,
        &[(
            12000,
            36000,
            AudioFormat::Ds2Sp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn ds2_olympus_sp_empty_qp() {
    decodes(
        "ds2/olympus-sp-empty-qp.ds2",
        None,
        &[(
            12000,
            36000,
            AudioFormat::Ds2Sp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn ds2_olympus_sp_hdr1() {
    decodes(
        "ds2/olympus-sp-hdr1.ds2",
        None,
        &[(
            12000,
            36000,
            AudioFormat::Ds2Sp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn ds2_olympus_sp_hdr17() {
    decodes(
        "ds2/olympus-sp-hdr17.ds2",
        None,
        &[(
            12000,
            36000,
            AudioFormat::Ds2Sp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn ds2_olympus_sp_qp_cut() {
    decodes(
        "ds2/olympus-sp-qp-cut.ds2",
        None,
        &[
            (
                12000,
                21312,
                AudioFormat::Ds2Sp,
                0,
                "0a207a576bae85ab45566d061268c008",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_sp_qp_format3_first() {
    decodes(
        "ds2/olympus-sp-qp-format3-first.ds2",
        None,
        &[
            (
                12000,
                36000,
                AudioFormat::Ds2Sp,
                0,
                "223897928c28b4a8edefd9938d518481",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_sp_qp_format3() {
    decodes(
        "ds2/olympus-sp-qp-format3.ds2",
        None,
        &[
            (
                12000,
                36000,
                AudioFormat::Ds2Sp,
                0,
                "223897928c28b4a8edefd9938d518481",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_sp_qp_resync() {
    decodes(
        "ds2/olympus-sp-qp-resync.ds2",
        None,
        &[
            (
                12000,
                36000,
                AudioFormat::Ds2Sp,
                0,
                "223897928c28b4a8edefd9938d518481",
            ),
            (
                16000,
                47616,
                AudioFormat::Ds2Qp,
                6,
                "815b9f3dcd72602ebb65635d98a78c31",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_sp_qp() {
    decodes(
        "ds2/olympus-sp-qp.ds2",
        None,
        &[
            (
                12000,
                36000,
                AudioFormat::Ds2Sp,
                0,
                "223897928c28b4a8edefd9938d518481",
            ),
            (
                16000,
                48128,
                AudioFormat::Ds2Qp,
                6,
                "c4d0fcd1e47d48b4ec8e85aed2fd7488",
            ),
        ],
    );
}

#[test]
fn ds2_olympus_sp() {
    decodes(
        "ds2/olympus-sp.ds2",
        None,
        &[(
            12000,
            120096,
            AudioFormat::Ds2Sp,
            0,
            "b42e989a4c162160557557e40b9362cb",
        )],
    );
}

#[test]
fn ds2_paused_qp() {
    decodes(
        "ds2/paused-qp.ds2",
        None,
        &[(
            16000,
            90112,
            AudioFormat::Ds2Qp,
            6,
            "294bb8fc3bb31ff141bc2aba74c15bac",
        )],
    );
}

#[test]
fn dss_olympus_format1_format_sp() {
    decodes(
        "dss/olympus-format1-format-sp.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            1,
            "29b4f756226dbdce7aa2450133098802",
        )],
    );
}

#[test]
fn dss_olympus_format1_hdr6_empty52() {
    decodes(
        "dss/olympus-format1-hdr6-empty52.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            1,
            "29b4f756226dbdce7aa2450133098802",
        )],
    );
}

#[test]
fn dss_olympus_format1_hdr6() {
    decodes(
        "dss/olympus-format1-hdr6.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            1,
            "29b4f756226dbdce7aa2450133098802",
        )],
    );
}

#[test]
fn dss_olympus_format1_header_copy() {
    decodes(
        "dss/olympus-format1-header-copy.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            1,
            "29b4f756226dbdce7aa2450133098802",
        )],
    );
}

#[test]
fn dss_olympus_format1_no_header() {
    same_as_original("dss/olympus-format1-no-header.dss", "dss/olympus-format1.dss");
}

#[test]
fn dss_olympus_format1_noswap_prev_empty60() {
    decodes(
        "dss/olympus-format1-noswap-prev-empty60.dss",
        None,
        &[(
            12000,
            5184,
            AudioFormat::DssSp,
            1,
            "d70f54a2efb506b9dc523aac1e3090e4",
        )],
    );
}

#[test]
fn dss_olympus_format1_noswap_prev() {
    decodes(
        "dss/olympus-format1-noswap-prev.dss",
        None,
        &[(
            12000,
            5184,
            AudioFormat::DssSp,
            1,
            "d70f54a2efb506b9dc523aac1e3090e4",
        )],
    );
}

#[test]
fn dss_olympus_format1_noswap_prev4_empty62() {
    decodes(
        "dss/olympus-format1-noswap-prev4-empty62.dss",
        None,
        &[(
            12000,
            5184,
            AudioFormat::DssSp,
            1,
            "d70f54a2efb506b9dc523aac1e3090e4",
        )],
    );
}

#[test]
fn dss_olympus_format1_noswap() {
    decodes(
        "dss/olympus-format1-noswap.dss",
        None,
        &[(
            12000,
            4896,
            AudioFormat::DssSp,
            1,
            "90a3f61823e61ac6d218d4737235a2e2",
        )],
    );
}

#[test]
fn dss_olympus_format1() {
    decodes(
        "dss/olympus-format1.dss",
        None,
        &[(
            12000,
            120096,
            AudioFormat::DssSp,
            1,
            "8bee9594fca3c81b0e9c81dcbec5c8b4",
        )],
    );
}

#[test]
fn dss_olympus_lp_format_sp() {
    decodes(
        "dss/olympus-lp-format-sp.dss",
        None,
        &[(
            8000,
            24000,
            AudioFormat::DssLp,
            2,
            "cb0ce2d909492c16a818557b1dcb29ba",
        )],
    );
}

#[test]
fn dss_olympus_lp_format4() {
    decodes(
        "dss/olympus-lp-format4.dss",
        None,
        &[(
            8000,
            24000,
            AudioFormat::DssLp,
            4,
            "cb0ce2d909492c16a818557b1dcb29ba",
        )],
    );
}

#[test]
fn dss_olympus_sp_byte2_ff() {
    decodes(
        "dss/olympus-sp-byte2-ff.dss",
        None,
        &[(
            12000,
            201600,
            AudioFormat::DssSp,
            0,
            "33a6a13cc810dcd2200ea81d4b891aaa",
        )],
    );
}

#[test]
fn dss_olympus_sp_byte3_mid() {
    decodes(
        "dss/olympus-sp-byte3-mid.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_byte3_none() {
    decodes(
        "dss/olympus-sp-byte3-none.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_count19() {
    decodes(
        "dss/olympus-sp-count19.dss",
        None,
        &[(
            12000,
            198144,
            AudioFormat::DssSp,
            0,
            "d4b91bb568bd390f9bc6ed37d8d68705",
        )],
    );
}

#[test]
fn dss_olympus_sp_empty_lp() {
    decodes(
        "dss/olympus-sp-empty-lp.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_empty56() {
    decodes(
        "dss/olympus-sp-empty56.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_format_ff() {
    decodes(
        "dss/olympus-sp-format-ff.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_format3_first() {
    decodes(
        "dss/olympus-sp-format3-first.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_hdr0() {
    decodes(
        "dss/olympus-sp-hdr0.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_hdr1() {
    decodes(
        "dss/olympus-sp-hdr1.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_no_date() {
    decodes(
        "dss/olympus-sp-no-date.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_pause_block() {
    decodes(
        "dss/olympus-sp-pause-block.dss",
        None,
        &[(
            12000,
            36000,
            AudioFormat::DssSp,
            0,
            "223897928c28b4a8edefd9938d518481",
        )],
    );
}

#[test]
fn dss_olympus_sp_philips_pause() {
    decodes(
        "dss/olympus-sp-philips-pause.dss",
        None,
        &[(
            12000,
            201600,
            AudioFormat::DssSp,
            0,
            "33a6a13cc810dcd2200ea81d4b891aaa",
        )],
    );
}

#[test]
fn dss_olympus_sp_swap_start() {
    decodes(
        "dss/olympus-sp-swap-start.dss",
        None,
        &[(
            12000,
            14400,
            AudioFormat::DssSp,
            0,
            "ea827eaffa9863d0aafc59212cc938c9",
        )],
    );
}
