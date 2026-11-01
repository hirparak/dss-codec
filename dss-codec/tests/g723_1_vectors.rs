//! Decodes the ITU-T G.723.1 test vectors and requires the reference output
//! sample for sample.
//!
//! The vectors are not in the repository. Set G723_1_VECTORS to the
//! directories that hold them, separated like PATH: the decoder vectors of
//! the Recommendation's Annex A software (DTX63.RCO, PATHD63P.TCO,
//! PATHD63P.CRC, PATHD63P.ROU, ...) and, for the cases that run the
//! postfilter both ways or erase frames at random, the extra reference
//! outputs (CODEC63_PF.ROU, RANDOM.TCO, ...). Without G723_1_VECTORS the
//! tests skip; a case whose files are not found skips with a message.
//!
//! Bitstream files hold frames back to back, each as long as the type in the
//! two low bits of its first byte says. An erasure file holds one 16-bit
//! little-endian flag per frame, non-zero for an erased frame. Output files
//! hold 16-bit little-endian samples.

use dss_codec::codec::g723_1::{frame_size, G7231Decoder};
use std::path::PathBuf;

fn find(name: &str) -> Option<PathBuf> {
    let dirs = std::env::var_os("G723_1_VECTORS")?;
    std::env::split_paths(&dirs)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn decode(bitstream: &[u8], erasures: Option<&[u8]>, postfilter: bool) -> Vec<u8> {
    let mut decoder = G7231Decoder::with_postfilter(postfilter);
    let mut out = Vec::new();
    let mut pos = 0;
    let mut erased = false;
    let mut index = 0;
    while pos < bitstream.len() {
        let end = (pos + frame_size(bitstream[pos])).min(bitstream.len());
        let frame = &bitstream[pos..end];
        pos = end;
        // When the erasure file runs out, its last flag holds.
        if let Some(flag) = erasures.and_then(|flags| flags.get(2 * index..2 * index + 2)) {
            erased = flag != [0, 0];
        }
        index += 1;
        let samples = if erased {
            decoder.decode_erased()
        } else {
            decoder.decode_frame(frame)
        };
        out.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
    }
    out
}

fn check(input: &str, erasures: Option<&str>, postfilter: bool, expected: &str) {
    if std::env::var_os("G723_1_VECTORS").is_none() {
        eprintln!("Skipping: set G723_1_VECTORS to the directories of the G.723.1 test vectors");
        return;
    }
    let mut names = vec![input, expected];
    names.extend(erasures);
    let Some(paths) = names
        .iter()
        .map(|name| find(name))
        .collect::<Option<Vec<_>>>()
    else {
        eprintln!("Skipping: {names:?} not all found in G723_1_VECTORS");
        return;
    };
    let read = |path: &PathBuf| std::fs::read(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let bitstream = read(&paths[0]);
    let reference = read(&paths[1]);
    let flags = paths.get(2).map(read);

    let output = decode(&bitstream, flags.as_deref(), postfilter);
    if let Some(at) = output
        .chunks(2)
        .zip(reference.chunks(2))
        .position(|(a, b)| a != b)
    {
        panic!(
            "{input}: sample {at} (frame {}) differs from {expected}",
            at / 240
        );
    }
    assert_eq!(output.len(), reference.len(), "{input}: output length");
}

macro_rules! vector {
    ($name:ident, $input:expr, $erasures:expr, $postfilter:expr, $expected:expr) => {
        #[test]
        fn $name() {
            check($input, $erasures, $postfilter, $expected);
        }
    };
}

// The decoder cases of the ITU test vectors. The 5.3 kbit/s main body
// vectors run with the postfilter off.
vector!(dtx63, "DTX63.RCO", None, true, "DTX63.ROU");
vector!(dtx53, "DTX53.RCO", None, true, "DTX53.ROU");
vector!(dtxmix, "DTXMIX.RCO", None, true, "DTXMIX.ROU");
vector!(dtx63e, "DTX63E.TCO", Some("DTX63E.CRC"), true, "DTX63E.ROU");
vector!(dtx63b, "DTX63B.TCO", None, true, "DTX63B.ROU");
vector!(
    pathd63p,
    "PATHD63P.TCO",
    Some("PATHD63P.CRC"),
    true,
    "PATHD63P.ROU"
);
vector!(
    tamed63p,
    "TAMED63P.TCO",
    Some("TAMED63P.CRC"),
    true,
    "TAMED63P.ROU"
);
vector!(overd63p, "OVERD63P.TCO", None, true, "OVERD63P.ROU");
vector!(pathd53, "PATHD53.TCO", None, false, "PATHD53.ROU");
vector!(overd53, "OVERD53.TCO", None, false, "OVERD53.ROU");
vector!(ineqd53, "INEQD53.TCO", None, false, "INEQD53.ROU");

// Extra reference outputs: the encoder vectors' bitstreams decoded with the
// postfilter on and off, burst erasures over comfort noise, and random bits.
vector!(codec63_pf, "CODEC63.RCO", None, true, "CODEC63_PF.ROU");
vector!(codec63_nopf, "CODEC63.RCO", None, false, "CODEC63_NOPF.ROU");
vector!(pathc63h_pf, "PATHC63H.RCO", None, true, "PATHC63H_PF.ROU");
vector!(
    pathc63h_nopf,
    "PATHC63H.RCO",
    None,
    false,
    "PATHC63H_NOPF.ROU"
);
vector!(pathc53_pf, "PATHC53.RCO", None, true, "PATHC53_PF.ROU");
vector!(pathc53_nopf, "PATHC53.RCO", None, false, "PATHC53_NOPF.ROU");
vector!(overc63_pf, "OVERC63.RCO", None, true, "OVERC63_PF.ROU");
vector!(overc63_nopf, "OVERC63.RCO", None, false, "OVERC63_NOPF.ROU");
vector!(overc53h_pf, "OVERC53H.RCO", None, true, "OVERC53H_PF.ROU");
vector!(
    overc53h_nopf,
    "OVERC53H.RCO",
    None,
    false,
    "OVERC53H_NOPF.ROU"
);
vector!(ineqc53_pf, "INEQC53.RCO", None, true, "INEQC53_PF.ROU");
vector!(ineqc53_nopf, "INEQC53.RCO", None, false, "INEQC53_NOPF.ROU");
vector!(tamec63h_pf, "TAMEC63H.RCO", None, true, "TAMEC63H_PF.ROU");
vector!(
    tamec63h_nopf,
    "TAMEC63H.RCO",
    None,
    false,
    "TAMEC63H_NOPF.ROU"
);
vector!(
    dtx63_eras,
    "DTX63.RCO",
    Some("DTX63_ERAS.CRC"),
    true,
    "DTX63_ERAS.ROU"
);
vector!(
    random_pf,
    "RANDOM.TCO",
    Some("RANDOM.CRC"),
    true,
    "RANDOM_PF.ROU"
);
vector!(
    random_nopf,
    "RANDOM.TCO",
    Some("RANDOM.CRC"),
    false,
    "RANDOM_NOPF.ROU"
);
