# DSS/DS2 Decoder

Open-source decoder for Olympus DSS and DS2 (DSS Pro) proprietary dictation audio formats. Converts `.dss` and `.ds2` files to standard WAV.

Also supports password-protected DS2 files when a password is provided.

The codec was fully reverse-engineered from Olympus's DssDecoder.dll using Ghidra. The decoder produces output matching the proprietary DLL (1.0000 correlation, bit-exact on all tested files).

## Quick Start

```bash
cd dss-codec
cargo build --release

# Convert a file
./target/release/dss-decode recording.DS2

# Resample to 16kHz (useful for ASR pipelines)
./target/release/dss-decode -r 16000 recording.DS2

# Decode an encrypted DS2 file with a password
./target/release/dss-decode --password 1234 recording.DS2

# Decode an encrypted DS2 file using an environment variable
DSS_CODEC_PASSWORD=1234 ./target/release/dss-decode recording.DS2

# Decrypt an encrypted DS2 file back to a plain .ds2 container
./target/release/dss-decode --decrypt --password 1234 recording.DS2
```

## Supported Formats

| File Type | Codec | Native Rate | Detection |
|-----------|-------|-------------|-----------|
| `.dss` (v2/v3) | DSS SP | 12000 Hz | Header `{00..ff}dss`, byte4 is 0 or 1 |
| `.ds2` mode 0-1 | DSS SP | 12000 Hz | Header `{01..ff}ds2`, byte4 is 0 or 1 |
| `.ds2` mode 6-7 | DS2 QP | 16000 Hz | Header `{01..ff}ds2`, byte4 is 6 or 7 |
| `.dss`, `.ds2` mode 2 or 4 | LP (G.723.1) | 8000 Hz | Header `{00..ff}dss` or `{01..ff}ds2`, byte4 is 2 or 4 |

The first byte of a DS2 file, 0x01 to 0xff, is its header length in
512-byte blocks. 0x01 means one block when byte 3 of the block at 0x200
is 0xff, as in an audio block header. Otherwise 0x01 means three blocks.
The first byte of a DSS file is its header length in blocks too, and
there 0x00 and 0x01 both mean one block. A DSS file with first byte 0x06
and codec byte 0 is read as a Grundig file.

DS2 SP files hold the same blocks and frames as DSS SP files and decode
the same way. Byte 4 of each audio block header is the block's format
when byte 3 is 0xff: 0 SP, 1 variable rate SP, 2 or 4 LP, 6 QP and 7
QP7. A block with another value there, or whose byte 3 is not 0xff,
takes the format of the last block before it that has frames, and a
block before any such block takes the format of the first block with
frames in a known format. A run of blocks in one format is a part, and
each part decodes with its own codec at its own rate. The first part
starts at the first frame the decoder reads, and its format and rate are
the file's. A file in which no block with frames has byte 3 set to 0xff
and a format byte in this list fails with `UnsupportedCodec`.

LP blocks hold ITU-T G.723.1 frames, decoded at 8000 Hz. A frame is 24,
20, 4 or 1 bytes long, as the two low bits of its first byte say: 6.3
kbit/s, 5.3 kbit/s, comfort noise (SID) or no data. As with QP, frames
run on from block to block and are not byte-swapped. Byte 2 of a block
header counts the frames that start in the block, or byte 5 does in a
pause block, and the anchor in bytes 0 and 1 is where the first of them
starts. A frame that does not end at the next block's anchor is dropped,
and reading resumes at the anchor.

Some files hold a stale copy of their header blocks right after the
header. A block there that starts with a "dss" or "ds2" header is such a
copy. The decoder skips it and the blocks that its first byte counts,
and reads the audio after them.

Encrypted DS2 files with header `\x03enc` are also supported when a password is provided, and can be either decoded directly or normalized back to a plain `.ds2` container with `--decrypt`.

## Project Structure

| Path | Description |
|------|-------------|
| `dss-codec/` | Rust crate — native decoder (CLI + library). ~140x faster than Python. |
| `dss_decode.py` | Python DSS SP reference decoder (requires numpy) |
| `ds2decode.py` | Python DS2 SP/QP reference decoder (requires numpy) |
| `ds2_lsp_codebook.npz` | SP reflection coefficient codebook (used by Python decoder) |
| `ds2_qp_codebook.npz` | QP reflection coefficient codebook (used by Python decoder) |
| `dss-codec/CODEC_SPECIFICATION.md` | Complete codec specification with all algorithms, tables, and DLL addresses |

## Background

FFmpeg's built-in `dss_sp` decoder does **not** work for DS2 files — the codec uses completely different parameters, tables, and sample rates. See [FFmpeg Trac #6091](https://trac.ffmpeg.org/ticket/6091) (open since 2017).

This project provides the first open-source implementation of the DS2 codec, verified against the proprietary Olympus DirectShow filters.

## Documentation

See [`dss-codec/CODEC_SPECIFICATION.md`](dss-codec/CODEC_SPECIFICATION.md) for the complete technical specification, including:

- File format and block structure
- Bitstream reader specification
- Frame bit allocations for all three codecs
- All algorithms in pseudocode (combinatorial codebook, pitch decoding, lattice synthesis)
- Complete quantization and codebook tables
- DLL function address map

## License

MIT — see [LICENSE](LICENSE).
