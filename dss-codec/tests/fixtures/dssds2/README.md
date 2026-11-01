# DSS and DS2 test fixtures

tests/fixtures_dssds2.rs decodes each of these 70 files. They are the
DSS and DS2 samples of FFmpeg's FATE set for the dssds2-v13 series whose
source we know and may share. ../grundig_sample.dss is that set's
dss/grundig-sp.dss, so this directory does not repeat it.

## Source and licence

63 files hold speech from "Composed upon Westminster Bridge" by William
Wordsworth, read for LibriVox. The archive.org item is
shortpoetry_026_librivox, and the reading is in the public domain. We
took 20.0 to 30.0 s of it, resampled it to the rate each mode takes, and
encoded it with the encoder and writer of Olympus ODMS R8, in trial
mode. The 10 s encodes are olympus-sp, olympus-format1, olympus-lp and
olympus-qp7. The other encodes hold the first 3 s. A second R8 writer
wrote the files whose format changes and the two encrypted files. The
password of both is test1234. We made the rest by hand from these
encodes, as the table says.

ds2/paused-qp.ds2 is Guillain d'Erceville's. He contributed it to FFmpeg
under CC0. It holds QP with a re-sync block and scrubbed audio, and it
ends inside a frame.

The two philips-pause files, olympus-qp-count19.ds2 and the files made
from them hold other speech. Each of those three is the first 700
frames of an R8 encode of test01_20s.wav from the voxserv
audio_quality_testing_samples set, QP in the .ds2 files and SP in the
.dss file. In a philips-pause file, three block headers come from a
list of pause blocks that Philips recorders write: the block before,
the pause block and the block after. A pause block gives its first
frame as a bit offset in the bottom half of byte 0 and in byte 3, and
its frame count in byte 5. The frames sit at that offset, as many as
the count says. olympus-sp-byte2-ff.dss has 0xff in byte 2 of one block
and that block's count in byte 5. A block with 0 or 0xff in byte 2
takes its count from byte 5. In
olympus-qp-count19.ds2 the middle block states 19 frames and holds 9,
like block 28 of paused-qp.ds2. olympus-qp-count19-byte3.ds2 is the
same file with 0 in byte 3 and 7 in byte 4 of that block. Its byte 4
does not count, so the limit of 13 applies, and the file decodes like
olympus-qp-count19.ds2. olympus-sp-count19.dss is
olympus-sp-philips-pause.dss with one block that states 19 frames and
holds 12. Format 0 gets the limit too, so that block holds no frames.

Formats: 0 is SP, 1 variable rate SP, 2 and 4 LP, 6 QP and 7 QP7. "3 s
SP" and the like name the plain 3 s encode of that format. Block numbers
count audio blocks from 0, from the first block after the file header.

| File | Made from | Edit or setting |
|------|-----------|-----------------|
| dss/olympus-format1.dss | 10 s format 1 encode | none, 21 short frames |
| dss/olympus-format1-format-sp.dss | 3 s format 1 | header format byte at 0x2a4 set to 0 |
| dss/olympus-format1-hdr6.dss | 3 s format 1 | R8 set to 6 header blocks |
| dss/olympus-format1-hdr6-empty52.dss | olympus-format1-hdr6.dss | 52 empty format 1 blocks in front |
| dss/olympus-format1-header-copy.dss | 3 s format 1 | R8 header copy, 2 header blocks, version 2 |
| dss/olympus-format1-no-header.dss | olympus-format1.dss | its 1024-byte header removed |
| dss/olympus-format1-noswap.dss | blocks 32 and 33 of olympus-format1.dss | the 2 bytes in front of the swapped first frame removed |
| dss/olympus-format1-noswap-prev.dss | olympus-format1-noswap.dss | after a block that states no frames and ends with the 2 bytes |
| dss/olympus-format1-noswap-prev-empty60.dss | olympus-format1-noswap-prev.dss | 60 empty format 1 blocks in front |
| dss/olympus-format1-noswap-prev4-empty62.dss | olympus-format1-noswap-prev.dss | the empty block ends with 4 bytes of the stream, 62 empty blocks in front |
| dss/olympus-lp-format4.dss | 3 s LP | R8 set to output media type 4 |
| dss/olympus-lp-format-sp.dss | 3 s LP | 0x2a4 set to 0 |
| dss/olympus-sp-byte2-ff.dss | olympus-sp-philips-pause.dss | audio block 31 set to 0f 15 ff ff 00 0c, its 12 frames in byte 5 |
| dss/olympus-sp-byte3-mid.dss | 3 s SP | 6th audio block: byte 3 set to 0, byte 4 to 2 |
| dss/olympus-sp-byte3-none.dss | 3 s SP | byte 3 set to 0 in every audio block |
| dss/olympus-sp-count19.dss | olympus-sp-philips-pause.dss | audio block 31 set to 0f 15 13 ff 00 ff, 19 frames stated, 12 held |
| dss/olympus-sp-empty56.dss | 3 s SP | 56 empty SP blocks in front |
| dss/olympus-sp-empty-lp.dss | 3 s SP | an empty LP block in front, 0x2a4 set to 0xff |
| dss/olympus-sp-format3-first.dss | 3 s SP | format byte of the first audio block set to 3 |
| dss/olympus-sp-format-ff.dss | 3 s SP | 0x2a4 set to 0xff |
| dss/olympus-sp-hdr0.dss | 3 s SP | first byte set to 0 |
| dss/olympus-sp-hdr1.dss | 3 s SP | first byte set to 1 |
| dss/olympus-sp-no-date.dss | 3 s SP | the 24 date bytes from 0x26 set to 0xff |
| dss/olympus-sp-pause-block.dss | 3 s SP | 6th audio block in pause form, its 13 frames in byte 5 |
| dss/olympus-sp-philips-pause.dss | 700 frames of an SP encode of test01 | audio blocks 15 to 17 are 0f 15 0c ff 00 ff, 01 00 00 78 00 0c and 8f 10 0c ff 00 ff |
| dss/olympus-sp-swap-start.dss | 3 s SP | its first 6 audio blocks removed |
| ds2/olympus-format1.ds2 | 10 s format 1 encode | none |
| ds2/olympus-format1-annot.ds2 | olympus-format1.ds2 | annotations at 0x400 |
| ds2/olympus-format1-header-copy.ds2 | 3 s format 1 | R8 header copy, 3 header blocks, version 0 |
| ds2/olympus-lp.ds2 | 10 s LP encode | none |
| ds2/olympus-lp-annot.ds2 | olympus-lp.ds2 | annotations at 0x400 |
| ds2/olympus-lp-empty64.ds2 | olympus-lp.ds2 | 64 empty LP blocks in front |
| ds2/olympus-lp-format4.ds2 | 3 s LP | R8 set to output media type 4 |
| ds2/olympus-qp7.ds2 | 10 s QP7 encode | none |
| ds2/olympus-qp7-empty64.ds2 | olympus-qp7.ds2 | 64 empty QP7 blocks in front |
| ds2/olympus-qp7-empty-lp.ds2 | 3 s QP7 | an empty LP block in front |
| ds2/olympus-qp7-lp.ds2 | 3 s QP7 and LP | second R8 writer: 21 QP7 blocks, then 5 LP blocks |
| ds2/olympus-qp7-lp-empty109.ds2 | olympus-qp7-lp.ds2 | 109 empty QP7 blocks in front |
| ds2/olympus-qp-aes-mode0.ds2 | 3 s QP | second R8 writer: AES-128, key size 0 |
| ds2/olympus-qp-aes256-mode0.ds2 | 3 s QP | second R8 writer: AES-256, key size 0 |
| ds2/olympus-qp-annot.ds2 | 3 s QP | annotations at 0x400 |
| ds2/olympus-qp-byte3-mid.ds2 | 3 s QP | 11th audio block: byte 3 set to 0, byte 4 to 7 |
| ds2/olympus-qp-byte3-none.ds2 | 3 s QP | byte 3 set to 0 in every audio block |
| ds2/olympus-qp-byte3-sp-first.ds2 | 3 s QP | 1st audio block: byte 3 set to 0, byte 4 to 0 |
| ds2/olympus-qp-count19.ds2 | 700 frames of a QP encode of test01 | audio blocks 41 to 43 are 0f 12 09 ff 06 ff, 0f 11 13 ff 06 ff and 0f 1b 09 ff 06 ff, as at a DS-2600 splice |
| ds2/olympus-qp-count19-byte3.ds2 | olympus-qp-count19.ds2 | audio block 42: byte 3 set to 0, byte 4 to 7 |
| ds2/olympus-qp-cut.ds2 | 3 s QP | cut 300 bytes into its 13th audio block |
| ds2/olympus-qp-format3-first.ds2 | 3 s QP | format byte of the first audio block set to 3 |
| ds2/olympus-qp-hdr1.ds2 | 3 s QP | R8 set to 1 header block |
| ds2/olympus-qp-hdr17.ds2 | 3 s QP | R8 set to 17 header blocks |
| ds2/olympus-qp-header-copy.ds2 | 3 s QP | R8 header copy, 3 header blocks, version 1 |
| ds2/olympus-qp-no-header.ds2 | 3 s QP | its 1536-byte header removed |
| ds2/olympus-qp-pause-block.ds2 | 3 s QP | 6th audio block in pause form, its 9 frames in byte 5 |
| ds2/olympus-qp-philips-pause.ds2 | 700 frames of a QP encode of test01 | audio blocks 39 to 41 are 0f 14 09 ff 06 ff, 01 00 00 c0 06 09 and 0f 1b 09 ff 06 ff |
| ds2/olympus-qp-qp7.ds2 | 3 s QP and QP7 | second R8 writer: 21 QP blocks, then 21 QP7 blocks |
| ds2/olympus-qp-qp7-format3.ds2 | olympus-qp-qp7.ds2 | 31st audio block, format 7 set to 3 |
| ds2/olympus-qp-resync.ds2 | 3 s QP | byte 1 of the 5th audio block raised from 27 to 55 |
| ds2/olympus-qp-tail-start.ds2 | 3 s QP | its first 6 audio blocks removed |
| ds2/olympus-sp.ds2 | 10 s SP encode | none |
| ds2/olympus-sp-annot.ds2 | olympus-sp.ds2 | annotations at 0x400 |
| ds2/olympus-sp-empty-lp.ds2 | 3 s SP | an empty LP block in front |
| ds2/olympus-sp-empty-qp.ds2 | 3 s SP | an empty QP block in front |
| ds2/olympus-sp-hdr1.ds2 | 3 s SP | R8 set to 1 header block |
| ds2/olympus-sp-hdr17.ds2 | 3 s SP | R8 set to 17 header blocks |
| ds2/olympus-sp-qp.ds2 | 3 s SP and QP | second R8 writer: 11 SP blocks, then 21 QP blocks |
| ds2/olympus-sp-qp-cut.ds2 | olympus-sp-qp.ds2 | SP blocks 7 to 11 removed |
| ds2/olympus-sp-qp-format3.ds2 | olympus-sp-qp.ds2 | 21st audio block, format 6 set to 3 |
| ds2/olympus-sp-qp-format3-first.ds2 | olympus-sp-qp.ds2 | 1st audio block, format 0 set to 3 |
| ds2/olympus-sp-qp-resync.ds2 | olympus-sp-qp.ds2 | byte 1 of the 16th audio block raised from 27 to 55, frame count of the 23rd from 9 to 10 |
| ds2/paused-qp.ds2 | Guillain d'Erceville, CC0 | QP with a re-sync block, ends inside a frame |

An empty block starts with 0f 03 00 ff, then the format byte and ff, and
506 zero bytes follow.

## Left out

These files of the FATE set have no licence we can pass on:

- ds2/sample-qp.ds2, ds2/aes128-qp.ds2 and ds2/aes256-qp.ds2, vendor
  test files from dictate.com.au.
- dss/sp-paused.dss, an excerpt of a real recording.

ds2/qp-resync-past-end.ds2 stays out as well. Its payload is random,
but its header is the 1536-byte header of ds2/sample-qp.ds2, from the
vendor set.

## What the tests check

For each file that dss-codec decodes, the test checks the number of
samples, the native rate, the format, the parts, and an md5 of the s16
PCM of each part at its native rate. The PCM is what dss-decode writes
to a WAV: each sample clamped to the 16-bit range, little endian. The
two encrypted files decode with the password test1234.

Before we wrote each md5 we checked that FFmpeg, built from the
dssds2-v13 series, decodes the file to the same samples. For a file
whose rate changes, FFmpeg's output at each part's rate holds that part
unresampled, and we compared the part there. The LP part of
ds2/olympus-qp7-lp.ds2 and ds2/olympus-qp7-lp-empty109.ds2 we compared
with FFmpeg's raw G.723.1 decoder instead, fed with the part's frames
(ffmpeg -f g723_1). The 8 files with LP blocks also decode in pieces to
the same parts as whole. One file differs from FFmpeg, and its test
checks something else:

- ds2/paused-qp.ds2 holds subframes with a fixed codebook index of
  C(64,11) or more. dss-codec clamps such an index to C(64,11) - 1.
  FFmpeg from the dssds2-v13 series decodes the index as it stands, so
  its decode differs. FFmpeg with the same clamp gives the md5 in the
  test.

FFmpeg refuses 2 files. olympus-format1-no-header.dss and
olympus-qp-no-header.ds2 have no header; dss-codec puts a blank one back
and decodes them to the same samples as olympus-format1.dss and
olympus-qp-annot.ds2, which their tests check.

A block's format counts only when byte 3 is 0xff and byte 4 names a
known format. In a file where no block's format counts, every block is
in the format of byte 4 of the first block with frames. So the two
byte3-none files decode like the encodes they come from.
FFmpeg with the same block header rules decodes them, the two
philips-pause files and olympus-sp-byte2-ff.dss to the md5 in each test.

## Regenerating a reference

For a file with one part, the md5 is that of the WAV data that
dss-decode writes, after the 44-byte header:

    dss-decode -q -O out.wav FILE
    tail -c +45 out.wav | md5sum

FFmpeg from the dssds2-v13 series gives the same md5:

    ffmpeg -v error -i FILE -f s16le - | md5sum

Add --password test1234 to dss-decode, or -password test1234 to ffmpeg,
for the two encrypted files. For a file with two rates, decode it with
ffmpeg -ar RATE for each part's rate. The part is a run of that output
with the length in the test, and the rest of the output is the other
part resampled.
