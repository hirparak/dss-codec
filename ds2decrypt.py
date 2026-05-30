#!/usr/bin/env python3
"""Decrypt Olympus encrypted DS2 files (magic \\x03enc).

Port of dss-codec-wasm crypto/ds2_encrypted.rs (AES-128/256 with per-block self-rekey).
"""

from __future__ import annotations

import hashlib
import struct
import sys
from dataclasses import dataclass
from enum import IntEnum
from typing import Union

ENCRYPTED_MAGIC = b"\x03enc"
PLAIN_MAGIC = b"\x03ds2"
DS2_HEADER_SIZE = 0x600
DS2_BLOCK_SIZE = 0x200
DS2_BLOCK_HEADER_SIZE = 6
DECRYPT_DESCRIPTOR_OFFSET = 0x146
DECRYPT_DESCRIPTOR_SIZE = 22
TRANSFORMED_BODY_SIZE = 0x1F0

SAVED_STATE_SIZE = 0x12C
CURRENT_BLOCK_OFFSET = 0x10
AES_STATE_OFFSET = 0x20
AES_ROUND_COUNT_OFFSET = 0x120
AES_FLAGS_OFFSET = 0x124
BLOCK_BYTE_INDEX_OFFSET = 0x128


class KeyMode(IntEnum):
    AES_128 = 1
    AES_256 = 2


@dataclass
class DecryptDescriptor:
    key_mode: KeyMode
    aux_16: bytes
    expected_check_word: int


class Ds2DecryptError(Exception):
    pass


def is_encrypted_ds2(data: bytes) -> bool:
    return len(data) >= 4 and data[:4] == ENCRYPTED_MAGIC


def inspect_encryption(data: bytes) -> dict:
    if not is_encrypted_ds2(data):
        return {"encrypted": False}
    desc = parse_decrypt_descriptor(data)
    mode = "ds2_aes_128" if desc.key_mode == KeyMode.AES_128 else "ds2_aes_256"
    return {
        "encrypted": True,
        "mode": mode,
        "key_mode": int(desc.key_mode),
        "expected_check": f"0x{desc.expected_check_word:04x}",
    }


def parse_decrypt_descriptor(data: bytes) -> DecryptDescriptor:
    end = DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE
    if len(data) < end:
        raise Ds2DecryptError("missing decrypt descriptor")
    raw = data[DECRYPT_DESCRIPTOR_OFFSET:end]
    mode_val = struct.unpack_from("<H", raw, 0)[0]
    if mode_val == 1:
        key_mode = KeyMode.AES_128
    elif mode_val == 2:
        key_mode = KeyMode.AES_256
    else:
        raise Ds2DecryptError(f"unsupported encrypted DS2 key mode {mode_val}")
    return DecryptDescriptor(
        key_mode=key_mode,
        aux_16=raw[2:18],
        expected_check_word=struct.unpack_from("<H", raw, 18)[0],
    )


def mix_password(password: bytes, aux_16: bytes) -> bytes:
    if len(password) > 16:
        raise Ds2DecryptError("password longer than 16 bytes is not supported")
    mixed = bytearray(16)
    mixed[: len(password)] = password
    for i in range(16):
        mixed[i] ^= aux_16[i]
    return bytes(mixed)


def derive_key_128(password: bytes, aux_16: bytes) -> tuple[bytes, int]:
    digest = hashlib.sha1(mix_password(password, aux_16)).digest()
    return digest[:16], struct.unpack_from("<H", digest, 16)[0]


def derive_key_256(password: bytes, aux_16: bytes) -> tuple[bytes, int]:
    digest = hashlib.sha384(mix_password(password, aux_16)).digest()
    return digest[:32], struct.unpack_from("<H", digest, 32)[0]


def swap_adjacent_bytes(buf: bytearray) -> None:
    for i in range(0, len(buf) - 1, 2):
        buf[i], buf[i + 1] = buf[i + 1], buf[i]


def _load_be32(b: bytes) -> int:
    return struct.unpack(">I", b)[0]


def _store_be32(word: int) -> bytes:
    return struct.pack(">I", word)


class PayloadDecryptState:
    def __init__(self) -> None:
        self.blob = bytearray(SAVED_STATE_SIZE)

    def clone(self) -> PayloadDecryptState:
        out = PayloadDecryptState()
        out.blob[:] = self.blob
        return out

    def round_key_word(self, index: int) -> int:
        off = AES_STATE_OFFSET + index * 4
        return _load_be32(bytes(self.blob[off : off + 4]))

    def set_round_key_word(self, index: int, word: int) -> None:
        off = AES_STATE_OFFSET + index * 4
        self.blob[off : off + 4] = _store_be32(word)

    def round_count(self) -> int:
        return struct.unpack_from("<I", self.blob, AES_ROUND_COUNT_OFFSET)[0]

    def set_round_count(self, rounds: int) -> None:
        struct.pack_into("<I", self.blob, AES_ROUND_COUNT_OFFSET, rounds)

    def set_flags(self, flags: int) -> None:
        struct.pack_into("<I", self.blob, AES_FLAGS_OFFSET, flags)

    def block_byte_index(self) -> int:
        return struct.unpack_from("<I", self.blob, BLOCK_BYTE_INDEX_OFFSET)[0]

    def set_block_byte_index(self, index: int) -> None:
        struct.pack_into("<I", self.blob, BLOCK_BYTE_INDEX_OFFSET, index)

    def current_block(self) -> bytes:
        return bytes(self.blob[CURRENT_BLOCK_OFFSET : CURRENT_BLOCK_OFFSET + 16])

    def set_current_block(self, block: bytes) -> None:
        self.blob[CURRENT_BLOCK_OFFSET : CURRENT_BLOCK_OFFSET + 16] = block

    def rekey_source_128(self) -> bytes:
        start = (self.round_count() + 2) * 0x10
        return bytes(self.blob[start : start + 16])

    def rekey_source_256(self) -> bytes:
        start = (self.round_count() + 2) * 0x10
        source = self.blob[start : start + 16]
        words = [_load_be32(source[i : i + 4]) for i in range(0, 16, 4)]
        words = [
            words[0],
            words[1],
            words[2],
            words[3],
            words[1],
            words[0],
            words[3],
            words[2],
        ]
        out = bytearray(32)
        for i, w in enumerate(words):
            out[i * 4 : i * 4 + 4] = _store_be32(w)
        return bytes(out)


AES_RCON = [
    0x01000000,
    0x02000000,
    0x04000000,
    0x08000000,
    0x10000000,
    0x20000000,
    0x40000000,
    0x80000000,
    0x1B000000,
    0x36000000,
]

AES_SBOX = bytes([
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
])

AES_INV_SBOX = bytes([
    0x52,0x09,0x6a,0xd5,0x30,0x36,0xa5,0x38,0xbf,0x40,0xa3,0x9e,0x81,0xf3,0xd7,0xfb,
    0x7c,0xe3,0x39,0x82,0x9b,0x2f,0xff,0x87,0x34,0x8e,0x43,0x44,0xc4,0xde,0xe9,0xcb,
    0x54,0x7b,0x94,0x32,0xa6,0xc2,0x23,0x3d,0xee,0x4c,0x95,0x0b,0x42,0xfa,0xc3,0x4e,
    0x08,0x2e,0xa1,0x66,0x28,0xd9,0x24,0xb2,0x76,0x5b,0xa2,0x49,0x6d,0x8b,0xd1,0x25,
    0x72,0xf8,0xf6,0x64,0x86,0x68,0x98,0x16,0xd4,0xa4,0x5c,0xcc,0x5d,0x65,0xb6,0x92,
    0x6c,0x70,0x48,0x50,0xfd,0xed,0xb9,0xda,0x5e,0x15,0x46,0x57,0xa7,0x8d,0x9d,0x84,
    0x90,0xd8,0xab,0x00,0x8c,0xbc,0xd3,0x0a,0xf7,0xe4,0x58,0x05,0xb8,0xb3,0x45,0x06,
    0xd0,0x2c,0x1e,0x8f,0xca,0x3f,0x0f,0x02,0xc1,0xaf,0xbd,0x03,0x01,0x13,0x8a,0x6b,
    0x3a,0x91,0x11,0x41,0x4f,0x67,0xdc,0xea,0x97,0xf2,0xcf,0xce,0xf0,0xb4,0xe6,0x73,
    0x96,0xac,0x74,0x22,0xe7,0xad,0x35,0x85,0xe2,0xf9,0x37,0xe8,0x1c,0x75,0xdf,0x6e,
    0x47,0xf1,0x1a,0x71,0x1d,0x29,0xc5,0x89,0x6f,0xb7,0x62,0x0e,0xaa,0x18,0xbe,0x1b,
    0xfc,0x56,0x3e,0x4b,0xc6,0xd2,0x79,0x20,0x9a,0xdb,0xc0,0xfe,0x78,0xcd,0x5a,0xf4,
    0x1f,0xdd,0xa8,0x33,0x88,0x07,0xc7,0x31,0xb1,0x12,0x10,0x59,0x27,0x80,0xec,0x5f,
    0x60,0x51,0x7f,0xa9,0x19,0xb5,0x4a,0x0d,0x2d,0xe5,0x7a,0x9f,0x93,0xc9,0x9c,0xef,
    0xa0,0xe0,0x3b,0x4d,0xae,0x2a,0xf5,0xb0,0xc8,0xeb,0xbb,0x3c,0x83,0x53,0x99,0x61,
    0x17,0x2b,0x04,0x7e,0xba,0x77,0xd6,0x26,0xe1,0x69,0x14,0x63,0x55,0x21,0x0c,0x7d,
])


def _rot_word(x: int) -> int:
    return ((x << 8) | (x >> 24)) & 0xFFFFFFFF


def _sub_word(x: int) -> int:
    return (
        (AES_SBOX[(x >> 24) & 0xFF] << 24)
        | (AES_SBOX[(x >> 16) & 0xFF] << 16)
        | (AES_SBOX[(x >> 8) & 0xFF] << 8)
        | AES_SBOX[x & 0xFF]
    )


def _gf_mul(a: int, b: int) -> int:
    result = 0
    while b:
        if b & 1:
            result ^= a
        carry = a & 0x80
        a = (a << 1) & 0xFF
        if carry:
            a ^= 0x1B
        b >>= 1
    return result


def aes_expand_key(state: PayloadDecryptState, key: bytes) -> None:
    if len(key) == 16:
        nk, nr = 4, 10
    elif len(key) == 24:
        nk, nr = 6, 12
    elif len(key) == 32:
        nk, nr = 8, 14
    else:
        raise Ds2DecryptError(f"unsupported AES key length {len(key)}")

    state.blob[AES_STATE_OFFSET:BLOCK_BYTE_INDEX_OFFSET] = b"\x00" * (
        BLOCK_BYTE_INDEX_OFFSET - AES_STATE_OFFSET
    )

    total_words = 4 * (nr + 1)
    for i in range(nk):
        state.set_round_key_word(i, _load_be32(key[i * 4 : i * 4 + 4]))

    for i in range(nk, total_words):
        temp = state.round_key_word(i - 1)
        if i % nk == 0:
            temp = _sub_word(_rot_word(temp)) ^ AES_RCON[(i // nk) - 1]
        elif nk > 6 and i % nk == 4:
            temp = _sub_word(temp)
        state.set_round_key_word(i, state.round_key_word(i - nk) ^ temp)

    state.set_round_count(nr)
    state.set_flags(0x12)


def _add_round_key(block: bytearray, state: PayloadDecryptState, word_index: int) -> None:
    for i in range(4):
        word = state.round_key_word(word_index + i)
        block[4 * i] ^= (word >> 24) & 0xFF
        block[4 * i + 1] ^= (word >> 16) & 0xFF
        block[4 * i + 2] ^= (word >> 8) & 0xFF
        block[4 * i + 3] ^= word & 0xFF


def _inv_sub_bytes(block: bytearray) -> None:
    for i in range(16):
        block[i] = AES_INV_SBOX[block[i]]


def _inv_shift_rows(block: bytearray) -> None:
    tmp = bytes(block)
    block[0], block[1], block[2], block[3] = tmp[0], tmp[13], tmp[10], tmp[7]
    block[4], block[5], block[6], block[7] = tmp[4], tmp[1], tmp[14], tmp[11]
    block[8], block[9], block[10], block[11] = tmp[8], tmp[5], tmp[2], tmp[15]
    block[12], block[13], block[14], block[15] = tmp[12], tmp[9], tmp[6], tmp[3]


def _inv_mix_columns(block: bytearray) -> None:
    for col in range(4):
        base = col * 4
        s0, s1, s2, s3 = block[base], block[base + 1], block[base + 2], block[base + 3]
        block[base] = _gf_mul(s0, 14) ^ _gf_mul(s1, 11) ^ _gf_mul(s2, 13) ^ _gf_mul(s3, 9)
        block[base + 1] = _gf_mul(s0, 9) ^ _gf_mul(s1, 14) ^ _gf_mul(s2, 11) ^ _gf_mul(s3, 13)
        block[base + 2] = _gf_mul(s0, 13) ^ _gf_mul(s1, 9) ^ _gf_mul(s2, 14) ^ _gf_mul(s3, 11)
        block[base + 3] = _gf_mul(s0, 11) ^ _gf_mul(s1, 13) ^ _gf_mul(s2, 9) ^ _gf_mul(s3, 14)


def aes_decrypt_block(state: PayloadDecryptState, ciphertext: bytes) -> bytes:
    round_count = state.round_count()
    block = bytearray(ciphertext)
    _add_round_key(block, state, 4 * round_count)
    for rnd in range(round_count - 1, 0, -1):
        _inv_shift_rows(block)
        _inv_sub_bytes(block)
        _add_round_key(block, state, 4 * rnd)
        _inv_mix_columns(block)
    _inv_shift_rows(block)
    _inv_sub_bytes(block)
    _add_round_key(block, state, 0)
    return bytes(block)


def build_saved_state(password: bytes, descriptor: DecryptDescriptor) -> PayloadDecryptState:
    if descriptor.key_mode == KeyMode.AES_128:
        derived_key, check = derive_key_128(password, descriptor.aux_16)
    else:
        derived_key, check = derive_key_256(password, descriptor.aux_16)

    if check != descriptor.expected_check_word:
        raise Ds2DecryptError(
            f"password rejected: expected 0x{descriptor.expected_check_word:04x}, "
            f"computed 0x{check:04x}"
        )

    state = PayloadDecryptState()
    state.set_block_byte_index(0x10)
    aes_expand_key(state, derived_key)
    return state


def decrypt_body_self_rekey(
    body: bytearray, state: PayloadDecryptState, key_mode: KeyMode
) -> None:
    if len(body) % 16 != 0:
        raise Ds2DecryptError(f"transformed body length {len(body)} is not a multiple of 16")

    for off in range(0, len(body), 16):
        chunk = body[off : off + 16]
        if state.block_byte_index() == 0x10:
            plaintext = aes_decrypt_block(state, chunk)
            state.set_current_block(plaintext)
            if key_mode == KeyMode.AES_128:
                rekey = state.rekey_source_128()
            else:
                rekey = state.rekey_source_256()
            aes_expand_key(state, rekey)
            state.set_block_byte_index(0)

        body[off : off + 16] = state.current_block()
        state.set_block_byte_index(0x10)


def decrypt_record_in_place(
    saved_state: PayloadDecryptState, key_mode: KeyMode, record: bytearray
) -> None:
    if len(record) != DS2_BLOCK_SIZE:
        raise Ds2DecryptError(f"expected {DS2_BLOCK_SIZE}-byte DS2 record, got {len(record)}")

    body_start = DS2_BLOCK_HEADER_SIZE
    body_end = body_start + TRANSFORMED_BODY_SIZE
    body = record[body_start:body_end]
    body_arr = bytearray(body)
    swap_adjacent_bytes(body_arr)

    state = saved_state.clone()
    decrypt_body_self_rekey(body_arr, state, key_mode)

    swap_adjacent_bytes(body_arr)
    record[body_start:body_end] = body_arr


def decrypt_encrypted_ds2(data: bytes, password: Union[str, bytes]) -> bytes:
    if isinstance(password, str):
        password = password.encode("utf-8")
    if len(data) < DS2_HEADER_SIZE:
        raise Ds2DecryptError("truncated encrypted DS2 header")
    if data[:4] != ENCRYPTED_MAGIC:
        raise Ds2DecryptError("expected encrypted DS2 magic \\x03enc")
    if len(password) > 16:
        raise Ds2DecryptError("password longer than 16 bytes is not supported")

    descriptor = parse_decrypt_descriptor(data)
    saved_state = build_saved_state(password, descriptor)

    out = bytearray(data)
    out[:4] = PLAIN_MAGIC

    offset = DS2_HEADER_SIZE
    while offset + DS2_BLOCK_SIZE <= len(out):
        block = bytearray(out[offset : offset + DS2_BLOCK_SIZE])
        decrypt_record_in_place(saved_state, descriptor.key_mode, block)
        out[offset : offset + DS2_BLOCK_SIZE] = block
        offset += DS2_BLOCK_SIZE

    return bytes(out)


def main() -> int:
    if len(sys.argv) >= 2 and sys.argv[1] == "--inspect":
        if len(sys.argv) != 3:
            print("Usage: ds2decrypt.py --inspect <encrypted.ds2>", file=sys.stderr)
            return 1
        data = open(sys.argv[2], "rb").read()
        print(inspect_encryption(data))
        return 0

    if len(sys.argv) != 4:
        print(
            "Usage: ds2decrypt.py <encrypted.ds2> <output.ds2> <password>\n"
            "       ds2decrypt.py --inspect <encrypted.ds2>",
            file=sys.stderr,
        )
        return 1

    inp, outp, pwd = sys.argv[1], sys.argv[2], sys.argv[3]
    plain = decrypt_encrypted_ds2(open(inp, "rb").read(), pwd)
    with open(outp, "wb") as f:
        f.write(plain)
    print(f"Decrypted {inp} -> {outp} ({len(plain)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
