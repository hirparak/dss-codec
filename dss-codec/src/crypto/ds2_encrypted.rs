use crate::error::{DecodeError, Result};
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha384;

pub const ENCRYPTED_MAGIC: [u8; 4] = *b"\x03enc";
pub const PLAIN_MAGIC: [u8; 4] = *b"\x03ds2";
pub const DS2_HEADER_SIZE: usize = 0x600;
pub const DS2_BLOCK_SIZE: usize = 0x200;
pub const DS2_BLOCK_HEADER_SIZE: usize = 6;
pub const DECRYPT_DESCRIPTOR_OFFSET: usize = 0x146;
pub const DECRYPT_DESCRIPTOR_SIZE: usize = 22;
pub const TRANSFORMED_BODY_SIZE: usize = 0x1f0;
const SAVED_STATE_SIZE: usize = 0x12c;
const CURRENT_BLOCK_OFFSET: usize = 0x10;
const AES_STATE_OFFSET: usize = 0x20;
const AES_ROUND_COUNT_OFFSET: usize = 0x120;
const AES_FLAGS_OFFSET: usize = 0x124;
const BLOCK_BYTE_INDEX_OFFSET: usize = 0x128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    Aes128 = 1,
    Aes256 = 2,
}

impl KeyMode {
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptDescriptor {
    pub key_mode: KeyMode,
    pub aux_16: [u8; 16],
    pub expected_check_word: u16,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugDecryptTrace {
    pub mode: KeyMode,
    pub expected_check_word: u16,
    pub saved_state: Vec<u8>,
    pub swapped_body: Vec<u8>,
    pub first_decrypted_16: [u8; 16],
    pub second_decrypted_16: [u8; 16],
    pub post_rekey_state_prefix: Vec<u8>,
    pub final_block: Vec<u8>,
}

#[derive(Clone)]
struct PayloadDecryptState {
    blob: [u8; SAVED_STATE_SIZE],
}

pub(crate) struct EncryptedDs2BlockDecryptor {
    password: Vec<u8>,
    header_buf: Vec<u8>,
    block_buf: Vec<u8>,
    saved_state: Option<PayloadDecryptState>,
    key_mode: Option<KeyMode>,
}

impl Default for PayloadDecryptState {
    fn default() -> Self {
        Self {
            blob: [0; SAVED_STATE_SIZE],
        }
    }
}

impl EncryptedDs2BlockDecryptor {
    pub(crate) fn new(password: &[u8]) -> Self {
        Self {
            password: password.to_vec(),
            header_buf: Vec::new(),
            block_buf: Vec::new(),
            saved_state: None,
            key_mode: None,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();

        if self.saved_state.is_none() {
            self.header_buf.extend_from_slice(data);

            if self.header_buf.len() >= ENCRYPTED_MAGIC.len()
                && !self.header_buf.starts_with(&ENCRYPTED_MAGIC)
            {
                return Err(DecodeError::EncryptedDs2(
                    "expected encrypted DS2 magic \\x03enc".to_string(),
                ));
            }

            if self.header_buf.len() < DS2_HEADER_SIZE {
                return Ok(out);
            }

            let descriptor = parse_decrypt_descriptor(&self.header_buf)?;
            let saved_state = build_saved_state(&self.password, &descriptor)?;

            self.saved_state = Some(saved_state);
            self.key_mode = Some(descriptor.key_mode);

            let mut plain_header = self.header_buf[..DS2_HEADER_SIZE].to_vec();
            plain_header[..4].copy_from_slice(&PLAIN_MAGIC);
            out.extend_from_slice(&plain_header);

            let remainder = self.header_buf.split_off(DS2_HEADER_SIZE);
            self.header_buf.truncate(DS2_HEADER_SIZE);
            self.block_buf.extend_from_slice(&remainder);
            return self.drain_full_blocks(out);
        }

        self.block_buf.extend_from_slice(data);
        self.drain_full_blocks(out)
    }

    fn drain_full_blocks(&mut self, mut out: Vec<u8>) -> Result<Vec<u8>> {
        while self.block_buf.len() >= DS2_BLOCK_SIZE {
            let mut block: Vec<u8> = self.block_buf.drain(..DS2_BLOCK_SIZE).collect();
            decrypt_record_in_place(
                self.saved_state.as_ref().unwrap(),
                self.key_mode.unwrap(),
                &mut block,
            )?;
            out.extend_from_slice(&block);
        }

        Ok(out)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<u8>> {
        if self.saved_state.is_none() {
            if self.header_buf.is_empty() {
                return Ok(Vec::new());
            }
            return Err(DecodeError::Truncated("encrypted DS2 header".to_string()));
        }

        if !self.block_buf.is_empty() {
            return Err(DecodeError::Truncated("encrypted DS2 block".to_string()));
        }

        Ok(Vec::new())
    }
}

impl PayloadDecryptState {
    fn round_key_word(&self, index: usize) -> u32 {
        let offset = AES_STATE_OFFSET + index * 4;
        load_be32(&self.blob[offset..offset + 4])
    }

    fn set_round_key_word(&mut self, index: usize, word: u32) {
        let offset = AES_STATE_OFFSET + index * 4;
        store_be32(&mut self.blob[offset..offset + 4], word);
    }

    fn round_count(&self) -> usize {
        u32::from_le_bytes(
            self.blob[AES_ROUND_COUNT_OFFSET..AES_ROUND_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        ) as usize
    }

    fn set_round_count(&mut self, rounds: usize) {
        self.blob[AES_ROUND_COUNT_OFFSET..AES_ROUND_COUNT_OFFSET + 4]
            .copy_from_slice(&(rounds as u32).to_le_bytes());
    }

    fn set_flags(&mut self, flags: u32) {
        self.blob[AES_FLAGS_OFFSET..AES_FLAGS_OFFSET + 4].copy_from_slice(&flags.to_le_bytes());
    }

    fn block_byte_index(&self) -> usize {
        u32::from_le_bytes(
            self.blob[BLOCK_BYTE_INDEX_OFFSET..BLOCK_BYTE_INDEX_OFFSET + 4]
                .try_into()
                .unwrap(),
        ) as usize
    }

    fn set_block_byte_index(&mut self, index: usize) {
        self.blob[BLOCK_BYTE_INDEX_OFFSET..BLOCK_BYTE_INDEX_OFFSET + 4]
            .copy_from_slice(&(index as u32).to_le_bytes());
    }

    fn current_block(&self) -> &[u8] {
        &self.blob[CURRENT_BLOCK_OFFSET..CURRENT_BLOCK_OFFSET + 16]
    }

    fn current_block_mut(&mut self) -> &mut [u8] {
        &mut self.blob[CURRENT_BLOCK_OFFSET..CURRENT_BLOCK_OFFSET + 16]
    }

    fn rekey_source_128(&self) -> [u8; 16] {
        let start = (self.round_count() + 2) * 0x10;
        self.blob[start..start + 16].try_into().unwrap()
    }

    fn rekey_source_256(&self) -> [u8; 32] {
        let start = (self.round_count() + 2) * 0x10;
        let source = &self.blob[start..start + 16];
        let mut words = [0u32; 8];
        for (i, chunk) in source.chunks_exact(4).enumerate() {
            words[i] = load_be32(chunk);
        }
        words[4] = words[1];
        words[5] = words[0];
        words[6] = words[3];
        words[7] = words[2];

        let mut out = [0u8; 32];
        for (i, word) in words.iter().enumerate() {
            store_be32(&mut out[i * 4..(i + 1) * 4], *word);
        }
        out
    }
}

pub fn parse_decrypt_descriptor(data: &[u8]) -> Result<DecryptDescriptor> {
    let raw = data
        .get(DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE)
        .ok_or_else(|| DecodeError::EncryptedDs2("missing decrypt descriptor".to_string()))?;

    let key_mode = match u16::from_le_bytes(raw[0..2].try_into().unwrap()) {
        1 => KeyMode::Aes128,
        2 => KeyMode::Aes256,
        mode => {
            return Err(DecodeError::EncryptedDs2(format!(
                "unsupported encrypted DS2 key mode {mode}"
            )))
        }
    };

    let mut aux_16 = [0u8; 16];
    aux_16.copy_from_slice(&raw[2..18]);

    Ok(DecryptDescriptor {
        key_mode,
        aux_16,
        expected_check_word: u16::from_le_bytes(raw[18..20].try_into().unwrap()),
    })
}

pub fn decrypt_encrypted_ds2(data: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    if data.len() < DS2_HEADER_SIZE {
        return Err(DecodeError::Truncated("encrypted DS2 header".to_string()));
    }
    if data[..4] != ENCRYPTED_MAGIC {
        return Err(DecodeError::EncryptedDs2(
            "expected encrypted DS2 magic \\x03enc".to_string(),
        ));
    }
    if password.len() > 16 {
        return Err(DecodeError::EncryptedDs2(
            "password longer than 16 bytes is not supported by current prototype".to_string(),
        ));
    }

    let descriptor = parse_decrypt_descriptor(data)?;
    let saved_state = build_saved_state(password, &descriptor)?;

    let mut out = data.to_vec();
    out[..4].copy_from_slice(&PLAIN_MAGIC);

    let mut offset = DS2_HEADER_SIZE;
    while offset + DS2_BLOCK_SIZE <= out.len() {
        decrypt_record_in_place(&saved_state, descriptor.key_mode, &mut out[offset..offset + DS2_BLOCK_SIZE])?;
        offset += DS2_BLOCK_SIZE;
    }

    Ok(out)
}

#[doc(hidden)]
pub fn debug_decrypt_block(
    data: &[u8],
    password: &[u8],
    block_index: usize,
) -> Result<DebugDecryptTrace> {
    if data.len() < DS2_HEADER_SIZE + DS2_BLOCK_SIZE {
        return Err(DecodeError::Truncated("encrypted DS2 block".to_string()));
    }
    if data[..4] != ENCRYPTED_MAGIC {
        return Err(DecodeError::EncryptedDs2(
            "expected encrypted DS2 magic \\x03enc".to_string(),
        ));
    }

    let descriptor = parse_decrypt_descriptor(data)?;
    let saved_state = build_saved_state(password, &descriptor)?;

    let block_offset = DS2_HEADER_SIZE + block_index * DS2_BLOCK_SIZE;
    let block = data
        .get(block_offset..block_offset + DS2_BLOCK_SIZE)
        .ok_or_else(|| DecodeError::Truncated("encrypted DS2 block index".to_string()))?;

    let mut final_block = block.to_vec();
    let mut swapped_body =
        final_block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_HEADER_SIZE + TRANSFORMED_BODY_SIZE].to_vec();
    swap_adjacent_bytes(&mut swapped_body);

    let mut state_for_core = saved_state.clone();
    let mut core_body = swapped_body.clone();
    decrypt_body_self_rekey(&mut core_body, &mut state_for_core, descriptor.key_mode)?;

    final_block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_HEADER_SIZE + TRANSFORMED_BODY_SIZE]
        .copy_from_slice(&core_body);
    swap_adjacent_bytes(
        &mut final_block[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_HEADER_SIZE + TRANSFORMED_BODY_SIZE],
    );

    let mut first_decrypted_16 = [0u8; 16];
    first_decrypted_16.copy_from_slice(&core_body[..16]);
    let mut second_decrypted_16 = [0u8; 16];
    second_decrypted_16.copy_from_slice(&core_body[16..32]);

    Ok(DebugDecryptTrace {
        mode: descriptor.key_mode,
        expected_check_word: descriptor.expected_check_word,
        saved_state: saved_state.blob.to_vec(),
        swapped_body,
        first_decrypted_16,
        second_decrypted_16,
        post_rekey_state_prefix: state_for_core.blob[..96].to_vec(),
        final_block,
    })
}

fn build_saved_state(password: &[u8], descriptor: &DecryptDescriptor) -> Result<PayloadDecryptState> {
    let (derived_key, computed_check_word) = match descriptor.key_mode {
        KeyMode::Aes128 => {
            let (key, check) = derive_key_128(password, &descriptor.aux_16)?;
            (key.to_vec(), check)
        }
        KeyMode::Aes256 => {
            let (key, check) = derive_key_256(password, &descriptor.aux_16)?;
            (key.to_vec(), check)
        }
    };

    if computed_check_word != descriptor.expected_check_word {
        return Err(DecodeError::EncryptedDs2(format!(
            "password rejected by descriptor check: expected 0x{:04x}, computed 0x{:04x}",
            descriptor.expected_check_word, computed_check_word
        )));
    }

    let mut state = PayloadDecryptState::default();
    state.set_block_byte_index(0x10);
    aes_expand_key(&mut state, &derived_key)?;
    Ok(state)
}

fn decrypt_record_in_place(
    saved_state: &PayloadDecryptState,
    key_mode: KeyMode,
    record: &mut [u8],
) -> Result<()> {
    if record.len() != DS2_BLOCK_SIZE {
        return Err(DecodeError::EncryptedDs2(format!(
            "expected {}-byte DS2 record, got {} bytes",
            DS2_BLOCK_SIZE,
            record.len()
        )));
    }

    let body = &mut record[DS2_BLOCK_HEADER_SIZE..DS2_BLOCK_HEADER_SIZE + TRANSFORMED_BODY_SIZE];
    swap_adjacent_bytes(body);

    let mut state = saved_state.clone();
    decrypt_body_self_rekey(body, &mut state, key_mode)?;

    swap_adjacent_bytes(body);
    Ok(())
}

fn derive_key_128(password: &[u8], aux_16: &[u8; 16]) -> Result<([u8; 16], u16)> {
    let mixed = mix_password(password, aux_16)?;
    let digest = Sha1::digest(mixed);

    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    let check_word = u16::from_le_bytes([digest[16], digest[17]]);
    Ok((key, check_word))
}

fn derive_key_256(password: &[u8], aux_16: &[u8; 16]) -> Result<([u8; 32], u16)> {
    let mixed = mix_password(password, aux_16)?;
    let digest = Sha384::digest(mixed);

    let mut key = [0u8; 32];
    key.copy_from_slice(&digest[..32]);
    let check_word = u16::from_le_bytes([digest[32], digest[33]]);
    Ok((key, check_word))
}

fn mix_password(password: &[u8], aux_16: &[u8; 16]) -> Result<[u8; 16]> {
    if password.len() > 16 {
        return Err(DecodeError::EncryptedDs2(
            "password longer than 16 bytes is not supported by current prototype".to_string(),
        ));
    }

    let mut mixed = [0u8; 16];
    mixed[..password.len()].copy_from_slice(password);
    for (dst, aux) in mixed.iter_mut().zip(aux_16) {
        *dst ^= *aux;
    }
    Ok(mixed)
}

fn swap_adjacent_bytes(bytes: &mut [u8]) {
    for pair in bytes.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
}

fn decrypt_body_self_rekey(
    body: &mut [u8],
    state: &mut PayloadDecryptState,
    key_mode: KeyMode,
) -> Result<()> {
    if body.len() % 16 != 0 {
        return Err(DecodeError::EncryptedDs2(format!(
            "transformed body length {} is not a multiple of 16",
            body.len()
        )));
    }

    for chunk in body.chunks_exact_mut(16) {
        if state.block_byte_index() == 0x10 {
            let plaintext = aes_decrypt_block(state, chunk)?;
            state.current_block_mut().copy_from_slice(&plaintext);

            match key_mode {
                KeyMode::Aes128 => {
                    let rekey = state.rekey_source_128();
                    aes_expand_key(state, &rekey)?;
                }
                KeyMode::Aes256 => {
                    let rekey = state.rekey_source_256();
                    aes_expand_key(state, &rekey)?;
                }
            }
            state.set_block_byte_index(0);
        }

        chunk.copy_from_slice(state.current_block());
        state.set_block_byte_index(0x10);
    }

    Ok(())
}

fn aes_expand_key(state: &mut PayloadDecryptState, key: &[u8]) -> Result<()> {
    let (nk, nr) = match key.len() {
        16 => (4, 10),
        24 => (6, 12),
        32 => (8, 14),
        len => {
            return Err(DecodeError::EncryptedDs2(format!(
                "unsupported AES key length {len}"
            )))
        }
    };

    for byte in &mut state.blob[AES_STATE_OFFSET..BLOCK_BYTE_INDEX_OFFSET] {
        *byte = 0;
    }

    let total_words = 4 * (nr + 1);
    for i in 0..nk {
        state.set_round_key_word(i, load_be32(&key[i * 4..i * 4 + 4]));
    }

    for i in nk..total_words {
        let mut temp = state.round_key_word(i - 1);
        if i % nk == 0 {
            temp = sub_word(rot_word(temp)) ^ AES_RCON[(i / nk) - 1];
        } else if nk > 6 && i % nk == 4 {
            temp = sub_word(temp);
        }
        let word = state.round_key_word(i - nk) ^ temp;
        state.set_round_key_word(i, word);
    }

    state.set_round_count(nr);
    state.set_flags(0x12);
    Ok(())
}

fn aes_decrypt_block(state: &PayloadDecryptState, ciphertext: &[u8]) -> Result<[u8; 16]> {
    if ciphertext.len() != 16 {
        return Err(DecodeError::EncryptedDs2(
            "AES block decrypt requires 16-byte input".to_string(),
        ));
    }

    let round_count = state.round_count();
    let mut block = [0u8; 16];
    block.copy_from_slice(ciphertext);

    add_round_key(&mut block, state, 4 * round_count);

    for round in (1..round_count).rev() {
        inv_shift_rows(&mut block);
        inv_sub_bytes(&mut block);
        add_round_key(&mut block, state, 4 * round);
        inv_mix_columns(&mut block);
    }

    inv_shift_rows(&mut block);
    inv_sub_bytes(&mut block);
    add_round_key(&mut block, state, 0);

    Ok(block)
}

fn add_round_key(state_bytes: &mut [u8; 16], state: &PayloadDecryptState, word_index: usize) {
    for i in 0..4 {
        let word = state.round_key_word(word_index + i);
        state_bytes[4 * i] ^= (word >> 24) as u8;
        state_bytes[4 * i + 1] ^= (word >> 16) as u8;
        state_bytes[4 * i + 2] ^= (word >> 8) as u8;
        state_bytes[4 * i + 3] ^= word as u8;
    }
}

fn inv_sub_bytes(state: &mut [u8; 16]) {
    for byte in state.iter_mut() {
        *byte = AES_INV_SBOX[*byte as usize];
    }
}

fn inv_shift_rows(state: &mut [u8; 16]) {
    let tmp = *state;
    state[0] = tmp[0];
    state[1] = tmp[13];
    state[2] = tmp[10];
    state[3] = tmp[7];
    state[4] = tmp[4];
    state[5] = tmp[1];
    state[6] = tmp[14];
    state[7] = tmp[11];
    state[8] = tmp[8];
    state[9] = tmp[5];
    state[10] = tmp[2];
    state[11] = tmp[15];
    state[12] = tmp[12];
    state[13] = tmp[9];
    state[14] = tmp[6];
    state[15] = tmp[3];
}

fn inv_mix_columns(state: &mut [u8; 16]) {
    for col in 0..4 {
        let base = col * 4;
        let s0 = state[base];
        let s1 = state[base + 1];
        let s2 = state[base + 2];
        let s3 = state[base + 3];
        state[base] = gf_mul(s0, 14) ^ gf_mul(s1, 11) ^ gf_mul(s2, 13) ^ gf_mul(s3, 9);
        state[base + 1] = gf_mul(s0, 9) ^ gf_mul(s1, 14) ^ gf_mul(s2, 11) ^ gf_mul(s3, 13);
        state[base + 2] = gf_mul(s0, 13) ^ gf_mul(s1, 9) ^ gf_mul(s2, 14) ^ gf_mul(s3, 11);
        state[base + 3] = gf_mul(s0, 11) ^ gf_mul(s1, 13) ^ gf_mul(s2, 9) ^ gf_mul(s3, 14);
    }
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut result = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            result ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    result
}

fn rot_word(x: u32) -> u32 {
    x.rotate_left(8)
}

fn sub_word(x: u32) -> u32 {
    ((AES_SBOX[((x >> 24) & 0xff) as usize] as u32) << 24)
        | ((AES_SBOX[((x >> 16) & 0xff) as usize] as u32) << 16)
        | ((AES_SBOX[((x >> 8) & 0xff) as usize] as u32) << 8)
        | (AES_SBOX[(x & 0xff) as usize] as u32)
}

fn load_be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

fn store_be32(bytes: &mut [u8], word: u32) {
    bytes.copy_from_slice(&word.to_be_bytes());
}

const AES_RCON: [u32; 10] = [
    0x0100_0000,
    0x0200_0000,
    0x0400_0000,
    0x0800_0000,
    0x1000_0000,
    0x2000_0000,
    0x4000_0000,
    0x8000_0000,
    0x1b00_0000,
    0x3600_0000,
];

const AES_SBOX: [u8; 256] = [
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
];

const AES_INV_SBOX: [u8; 256] = [
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
];

#[cfg(test)]
mod tests {
    use super::*;

    const DESC_128: [u8; 22] = [
        0x01, 0x00, 0xEA, 0x89, 0x41, 0x86, 0x20, 0x9E, 0x20, 0xF3, 0xCD, 0x63, 0xF4, 0xD9,
        0x34, 0xF0, 0xD3, 0x8D, 0x10, 0xC9, 0xD2, 0x06,
    ];
    const DESC_256: [u8; 22] = [
        0x02, 0x00, 0x23, 0xBC, 0xB6, 0xA9, 0x99, 0x81, 0x20, 0x83, 0x39, 0xF3, 0xA4, 0xC3,
        0xBF, 0x41, 0xEE, 0x5A, 0xA7, 0xA8, 0x2A, 0x12,
    ];

    #[test]
    fn parse_descriptor_128() {
        let mut header = [0u8; DS2_HEADER_SIZE];
        header[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_128);
        let desc = parse_decrypt_descriptor(&header).unwrap();
        assert_eq!(desc.key_mode, KeyMode::Aes128);
        assert_eq!(desc.expected_check_word, 0xC910);
        assert_eq!(
            desc.aux_16,
            [
                0xEA, 0x89, 0x41, 0x86, 0x20, 0x9E, 0x20, 0xF3, 0xCD, 0x63, 0xF4, 0xD9, 0x34,
                0xF0, 0xD3, 0x8D
            ]
        );
    }

    #[test]
    fn derive_128_matches_known_sample_descriptor() {
        let (_, check) = derive_key_128(b"1234", &[
            0xEA, 0x89, 0x41, 0x86, 0x20, 0x9E, 0x20, 0xF3, 0xCD, 0x63, 0xF4, 0xD9, 0x34,
            0xF0, 0xD3, 0x8D,
        ]).unwrap();
        assert_eq!(check, 0xC910);
    }

    #[test]
    fn derive_256_matches_known_sample_descriptor() {
        let (_, check) = derive_key_256(b"1234", &[
            0x23, 0xBC, 0xB6, 0xA9, 0x99, 0x81, 0x20, 0x83, 0x39, 0xF3, 0xA4, 0xC3, 0xBF,
            0x41, 0xEE, 0x5A,
        ]).unwrap();
        assert_eq!(check, 0xA8A7);
    }

    #[test]
    fn build_saved_state_accepts_known_128_password() {
        let mut header = [0u8; DS2_HEADER_SIZE];
        header[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_128);
        let desc = parse_decrypt_descriptor(&header).unwrap();
        build_saved_state(b"1234", &desc).unwrap();
    }

    #[test]
    fn build_saved_state_accepts_known_256_password() {
        let mut header = [0u8; DS2_HEADER_SIZE];
        header[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_256);
        let desc = parse_decrypt_descriptor(&header).unwrap();
        build_saved_state(b"1234", &desc).unwrap();
    }

    #[test]
    fn encrypted_stream_decryptor_waits_for_full_header() {
        let mut input = vec![0u8; DS2_HEADER_SIZE];
        input[..4].copy_from_slice(&ENCRYPTED_MAGIC);
        input[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_128);

        let mut decryptor = EncryptedDs2BlockDecryptor::new(b"1234");
        assert!(decryptor.push(&input[..128]).unwrap().is_empty());
        let plain = decryptor.push(&input[128..]).unwrap();
        assert_eq!(&plain[..4], &PLAIN_MAGIC);
        assert_eq!(plain.len(), DS2_HEADER_SIZE);
    }

    #[test]
    fn encrypted_stream_decryptor_rejects_wrong_password_after_header() {
        let mut input = vec![0u8; DS2_HEADER_SIZE];
        input[..4].copy_from_slice(&ENCRYPTED_MAGIC);
        input[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_128);

        let mut decryptor = EncryptedDs2BlockDecryptor::new(b"9999");
        let err = decryptor.push(&input).unwrap_err();
        assert!(matches!(err, DecodeError::EncryptedDs2(_)));
    }

    #[test]
    fn encrypted_stream_decryptor_finish_rejects_truncated_header() {
        let mut decryptor = EncryptedDs2BlockDecryptor::new(b"1234");
        let _ = decryptor.push(b"\x03enc").unwrap();
        let err = decryptor.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn encrypted_stream_decryptor_finish_rejects_truncated_block() {
        let mut input = vec![0u8; DS2_HEADER_SIZE];
        input[..4].copy_from_slice(&ENCRYPTED_MAGIC);
        input[DECRYPT_DESCRIPTOR_OFFSET..DECRYPT_DESCRIPTOR_OFFSET + DECRYPT_DESCRIPTOR_SIZE]
            .copy_from_slice(&DESC_128);

        let mut decryptor = EncryptedDs2BlockDecryptor::new(b"1234");
        let _ = decryptor.push(&input).unwrap();
        let _ = decryptor.push(&[0u8; 10]).unwrap();
        let err = decryptor.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }
}
