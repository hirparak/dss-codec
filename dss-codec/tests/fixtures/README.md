# Test fixtures

These files exist solely for codec regression testing.

## Grundig DSS-SP

- `grundig_sample.dss`
- `grundig_sample_16k.wav`

These fixtures were contributed by Guillain-RDCDE in upstream pull request
[hirparak/dss-codec#12](https://github.com/hirparak/dss-codec/pull/12). The WAV
was produced by the genuine Grundig DigtaSoft reference decoder. The integration
test requires this project to reproduce it byte-for-byte.

## Encrypted Olympus DS2

- `encrypted_aes128.ds2`
- `encrypted_aes128_reference.wav`
- `encrypted_aes256.ds2`
- `encrypted_aes256_reference.wav`

The encoded files were recovered from the encrypted-DS2 development and
reverse-engineering workspace. Their original filenames were:

- `Sample_DS2_Audio_File_-_128bit_Encryption_-_Password_is_1234.ds2`
- `Sample_DS2_Audio_File_-_256bit_Encryption_-_Password_is_1234.ds2`

Both use the password `1234`. The original download location was not recorded,
so no stronger provenance claim is made here.

The corresponding WAV files are reviewed outputs from this implementation and
serve as exact regression baselines. They ensure that decryption and decoding
remain byte-stable across code and dependency changes; unlike the Grundig WAV,
they are not independently verified vendor-decoder outputs.
