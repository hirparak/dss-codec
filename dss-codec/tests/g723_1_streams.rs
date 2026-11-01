//! G.723.1 packet streams checked against FFmpeg's raw G.723.1 decoder.
//!
//! `negative-lsp.g723pk` holds eight valid 6.3 kbit/s frames, each a u16
//! little-endian length followed by the frame, sent by Patrick Domack. They
//! pull the first LSP below zero, and the decoder used to panic on them.
//! `negative-lsp.s16le` is FFmpeg's output for the same frames
//! (`ffmpeg -f g723_1 -i frames -f s16le -`).

use dss_codec::codec::g723_1::G7231Decoder;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn packets(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= data.len() {
        let len = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        out.push(&data[i + 2..i + 2 + len]);
        i += 2 + len;
    }
    out
}

#[test]
fn a_negative_lsp_decodes_like_ffmpeg() {
    let stream = fixture("negative-lsp.g723pk");
    let mut decoder = G7231Decoder::new();
    let pcm: Vec<u8> = packets(&stream)
        .into_iter()
        .flat_map(|frame| decoder.decode_frame(frame))
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(pcm, fixture("negative-lsp.s16le"));
}
