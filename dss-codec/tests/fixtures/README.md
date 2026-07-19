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

The encoded files are the free DSS Pro samples published by Dictate Australia
on 13 June 2018:

<https://dictate.com.au/blogs/news/download-ds2-audio-file-samples-dss-pro>

Their original filenames were:

- `Sample_DS2_Audio_File_-_128bit_Encryption_-_Password_is_1234.ds2`
- `Sample_DS2_Audio_File_-_256bit_Encryption_-_Password_is_1234.ds2`

Both use the password `1234`, as documented on the source page. Fresh downloads
were compared with these fixtures byte-for-byte:

| Fixture | SHA-256 |
|---|---|
| `encrypted_aes128.ds2` | `f83a7133b763ccee812bf40aaa25b69f27c701d7e27742d1242ecabd40545802` |
| `encrypted_aes256.ds2` | `c9f9ad618c4a3243ea8884e0deae0a4c88af6ad0494030edbed30c054396df35` |

The corresponding WAV files are reviewed outputs from this implementation and
serve as exact regression baselines. They ensure that decryption and decoding
remain byte-stable across code and dependency changes; unlike the Grundig WAV,
they are not independently verified vendor-decoder outputs.
