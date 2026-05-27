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
| `.dss` (v2/v3) | DSS SP | 11025 Hz | Header `{02\|03}dss` |
| `.ds2` mode 0-1 | DS2 SP | 12000 Hz | Header `\x03ds2`, byte4 < 6 |
| `.ds2` mode 6-7 | DS2 QP | 16000 Hz | Header `\x03ds2`, byte4 >= 6 |

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
