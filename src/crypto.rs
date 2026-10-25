use aes::Aes128;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher};
use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose};
use cbc::{Decryptor as CbcDec, Encryptor as CbcEnc};
use cipher::block_padding::{NoPadding, Pkcs7};
use ctr::Ctr128BE;
use num_bigint::BigUint;
use rsa::{RsaPrivateKey, traits::PrivateKeyParts, traits::PublicKeyParts};

type Aes128CbcEnc = CbcEnc<Aes128>;
type Aes128CbcDec = CbcDec<Aes128>;
type Aes128Ctr = Ctr128BE<Aes128>;

// --- Base64 ---

pub fn url_base64_decode(data: &str) -> Result<Vec<u8>> {
    Ok(general_purpose::URL_SAFE_NO_PAD.decode(data)?)
}

pub fn url_base64_encode(data: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[allow(dead_code)]
pub fn base64_decode(data: &str) -> Result<Vec<u8>> {
    Ok(general_purpose::STANDARD.decode(data)?)
}

#[allow(dead_code)]
pub fn base64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

// --- i32 array <-> bytes (big-endian, MEGA style) ---

pub fn bin_to_i32a(bin: &[u8]) -> Vec<i32> {
    let padded_len = ((bin.len() + 3) / 4) * 4;
    let mut padded = vec![0u8; padded_len];
    padded[..bin.len()].copy_from_slice(bin);
    padded
        .chunks(4)
        .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn i32a_to_bin(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

// --- AES-ECB ---

pub fn aes_ecb_encrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    let key_arr: [u8; 16] = key[..16].try_into()?;
    let mut cipher = Aes128::new(&key_arr.into());
    let mut result = data.to_vec();
    // Pad to 16 bytes
    let padded_len = ((result.len() + 15) / 16) * 16;
    result.resize(padded_len, 0);
    for chunk in result.chunks_mut(16) {
        let block = aes::Block::from_mut_slice(chunk);
        cipher.encrypt_block_mut(block);
    }
    Ok(result)
}

pub fn aes_ecb_decrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    let key_arr: [u8; 16] = key[..16].try_into()?;
    let mut cipher = Aes128::new(&key_arr.into());
    let mut result = data.to_vec();
    let padded_len = ((result.len() + 15) / 16) * 16;
    result.resize(padded_len, 0);
    for chunk in result.chunks_mut(16) {
        let block = aes::Block::from_mut_slice(chunk);
        cipher.decrypt_block_mut(block);
    }
    Ok(result)
}

// --- AES-CBC ---

pub fn aes_cbc_encrypt_nopadding(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    let mut buf = data.to_vec();
    let padded_len = ((buf.len() + 15) / 16) * 16;
    buf.resize(padded_len, 0);
    let ct = Aes128CbcEnc::new(key[..16].into(), iv[..16].into())
        .encrypt_padded_mut::<NoPadding>(&mut buf, padded_len)
        .map_err(|e| anyhow!("CBC encrypt: {}", e))?;
    Ok(ct.to_vec())
}

pub fn aes_cbc_decrypt_nopadding(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    let mut buf = data.to_vec();
    let pt = Aes128CbcDec::new(key[..16].into(), iv[..16].into())
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .map_err(|e| anyhow!("CBC decrypt: {}", e))?;
    Ok(pt.to_vec())
}

#[allow(dead_code)]
pub fn aes_cbc_decrypt_pkcs7(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    let mut buf = data.to_vec();
    let pt = Aes128CbcDec::new(key[..16].into(), iv[..16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow!("CBC decrypt pkcs7: {}", e))?;
    Ok(pt.to_vec())
}

#[allow(dead_code)]
pub fn aes_cbc_encrypt_pkcs7(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    let block_len = ((data.len() + 16) / 16) * 16;
    let mut buf = vec![0u8; block_len];
    buf[..data.len()].copy_from_slice(data);
    let ct = Aes128CbcEnc::new(key[..16].into(), iv[..16].into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .map_err(|e| anyhow!("CBC encrypt pkcs7: {}", e))?;
    Ok(ct.to_vec())
}

// --- AES-CBC for i32 arrays (MEGA login) ---

pub fn aes_cbc_encrypt_i32a(data: &[i32; 4], key: &[i32; 4], iv: &[i32; 4]) -> Result<[i32; 4]> {
    let data_bin = i32a_to_bin(data);
    let key_bin = i32a_to_bin(key);
    let iv_bin = i32a_to_bin(iv);
    let enc = aes_cbc_encrypt_nopadding(&data_bin, &key_bin, &iv_bin)?;
    let result = bin_to_i32a(&enc);
    Ok([result[0], result[1], result[2], result[3]])
}

// --- AES-CTR (for file decryption) ---

#[allow(dead_code)]
pub fn aes_ctr_decrypt(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    let mut cipher = Aes128Ctr::new(key[..16].into(), iv[..16].into());
    let mut result = data.to_vec();
    cipher.apply_keystream(&mut result);
    Ok(result)
}

pub fn make_aes_ctr_cipher(key: &[u8], iv: &[u8]) -> Aes128Ctr {
    Aes128Ctr::new(key[..16].into(), iv[..16].into())
}

// --- MEGA link key derivation ---

pub fn init_mega_link_key(key_string: &str) -> Result<Vec<u8>> {
    let raw = url_base64_decode(key_string)?;
    let int_key = bin_to_i32a(&raw);
    if int_key.len() < 8 {
        return Err(anyhow!("Invalid key length"));
    }
    let k = [
        int_key[0] ^ int_key[4],
        int_key[1] ^ int_key[5],
        int_key[2] ^ int_key[6],
        int_key[3] ^ int_key[7],
    ];
    Ok(i32a_to_bin(&k))
}

pub fn init_mega_link_key_iv(key_string: &str) -> Result<Vec<u8>> {
    let raw = url_base64_decode(key_string)?;
    let int_key = bin_to_i32a(&raw);
    if int_key.len() < 6 {
        return Err(anyhow!("Invalid key length for IV"));
    }
    let iv: [i32; 4] = [int_key[4], int_key[5], 0, 0];
    Ok(i32a_to_bin(&iv))
}

/// Derive the 16-byte AES key from a decrypted MEGA node key.
///
/// A node key is 32 bytes: the AES key is the first half XOR-ed with the second
/// half. Legacy 16-byte node keys are used as-is.
pub fn node_aes_key(dec_key: &[u8]) -> Vec<u8> {
    if dec_key.len() >= 32 {
        let words = bin_to_i32a(&dec_key[..32]);
        i32a_to_bin(&[
            words[0] ^ words[4],
            words[1] ^ words[5],
            words[2] ^ words[6],
            words[3] ^ words[7],
        ])
    } else {
        dec_key[..dec_key.len().min(16)].to_vec()
    }
}

pub fn forward_mega_link_key_iv(iv: &[u8], forward_bytes: u64) -> Vec<u8> {
    let mut new_iv = vec![0u8; iv.len()];
    let half = iv.len() / 2;
    new_iv[..half].copy_from_slice(&iv[..half]);
    let ctr = (forward_bytes / iv.len() as u64).to_be_bytes();
    new_iv[half..half + 8].copy_from_slice(&ctr);
    new_iv
}

// --- URL base64 key decode (for folder keys) ---

pub fn url_base64_key_decode(key: &str) -> Result<Vec<u8>> {
    let key_bin = url_base64_decode(key)?;
    if key_bin.len() < 32 {
        Ok(key_bin[..16.min(key_bin.len())].to_vec())
    } else {
        let key_i32a = bin_to_i32a(&key_bin[..32]);
        let k = [
            key_i32a[0] ^ key_i32a[4],
            key_i32a[1] ^ key_i32a[5],
            key_i32a[2] ^ key_i32a[6],
            key_i32a[3] ^ key_i32a[7],
        ];
        Ok(i32a_to_bin(&k))
    }
}

// --- MEGA prepare master key (legacy login) ---

pub fn mega_prepare_master_key(key: &[i32]) -> Result<[i32; 4]> {
    let mut pkey: [i32; 4] = [0x93C467E3u32 as i32, 0x7DB0C7A4u32 as i32, 0xD1BE3F81u32 as i32, 0x0152CB56u32 as i32];
    let iv: [i32; 4] = [0, 0, 0, 0];

    for _ in 0..0x10000 {
        let mut j = 0;
        while j < key.len() {
            let mut k = [0i32; 4];
            for i in 0..4 {
                if i + j < key.len() {
                    k[i] = key[i + j];
                }
            }
            pkey = aes_cbc_encrypt_i32a(&pkey, &k, &iv)?;
            j += 4;
        }
    }
    Ok(pkey)
}

// --- MEGA user hash (legacy login) ---

pub fn mega_user_hash(email_bytes: &[u8], aeskey: &[i32; 4]) -> Result<String> {
    let s32 = bin_to_i32a(email_bytes);
    let mut h32 = [0i32; 4];
    let iv = [0i32; 4];

    for (i, &val) in s32.iter().enumerate() {
        h32[i % 4] ^= val;
    }

    for _ in 0..0x4000 {
        h32 = aes_cbc_encrypt_i32a(&h32, aeskey, &iv)?;
    }

    let res = [h32[0], h32[2]];
    Ok(url_base64_encode(&i32a_to_bin(&res)))
}

// --- PBKDF2 ---

pub fn pbkdf2_hmac_sha512(password: &str, salt: &[u8], iterations: u32, output_len: usize) -> Vec<u8> {
    use hmac::Hmac;
    use sha2::Sha512;

    let mut result = vec![0u8; output_len];
    pbkdf2::pbkdf2::<Hmac<Sha512>>(password.as_bytes(), salt, iterations, &mut result)
        .expect("PBKDF2 failed");
    result
}

// --- RSA decrypt ---

pub fn rsa_decrypt(enc_data: &BigUint, p: &BigUint, q: &BigUint, d: &BigUint) -> Result<Vec<u8>> {
    let n = p * q;
    let priv_key = RsaPrivateKey::from_components(
        rsa::BigUint::from_bytes_be(&n.to_bytes_be()),
        rsa::BigUint::from(65537u32),
        rsa::BigUint::from_bytes_be(&d.to_bytes_be()),
        vec![
            rsa::BigUint::from_bytes_be(&p.to_bytes_be()),
            rsa::BigUint::from_bytes_be(&q.to_bytes_be()),
        ],
    )?;

    let enc_bytes = enc_data.to_bytes_be();
    let n_key = priv_key.n();
    let d_key = priv_key.d();

    // Raw RSA decryption: plaintext = enc_data^d mod n
    let enc_biguint = rsa::BigUint::from_bytes_be(&enc_bytes);
    let plain = enc_biguint.modpow(d_key, n_key);
    let mut result = plain.to_bytes_be();

    // Strip leading zero if present
    if !result.is_empty() && result[0] == 0 {
        result = result[1..].to_vec();
    }

    Ok(result)
}

// --- MPI to BigUint ---

pub fn mpi_to_big(s: &[u8]) -> BigUint {
    if s.len() < 2 {
        return BigUint::from(0u32);
    }
    BigUint::from_bytes_be(&s[2..])
}

// --- Extract RSA private key from decrypted privk ---

pub fn extract_rsa_priv_key(rsa_data: &[u8]) -> Result<[BigUint; 4]> {
    let mut keys = Vec::new();
    let mut offset = 0;

    for _ in 0..4 {
        if offset + 1 >= rsa_data.len() {
            return Err(anyhow!("RSA key data too short"));
        }
        let bit_len = 256 * (rsa_data[offset] as usize & 0xFF) + (rsa_data[offset + 1] as usize & 0xFF);
        let byte_len = (bit_len + 7) / 8;
        let l = byte_len + 2;
        if offset + l > rsa_data.len() {
            return Err(anyhow!("RSA key data too short at component"));
        }
        keys.push(mpi_to_big(&rsa_data[offset..offset + l]));
        offset += l;
    }

    Ok([
        keys[0].clone(),
        keys[1].clone(),
        keys[2].clone(),
        keys[3].clone(),
    ])
}

// --- Decrypt attributes ---

pub fn decrypt_mega_attr(enc_attr: &str, key: &[u8]) -> Result<serde_json::Value> {
    let zero_iv = [0u8; 16];
    let data = url_base64_decode(enc_attr)?;
    let decrypted = aes_cbc_decrypt_nopadding(&data, key, &zero_iv)?;
    let s = String::from_utf8_lossy(&decrypted);
    let s = s.trim_end_matches('\0');
    let s = s.strip_prefix("MEGA").unwrap_or(s);
    let val: serde_json::Value = serde_json::from_str(s)?;
    Ok(val)
}

// --- Encrypt attributes ---

pub fn encrypt_mega_attr(attr_json: &str, key: &[u8]) -> Result<Vec<u8>> {
    let zero_iv = [0u8; 16];
    let attr_bytes = format!("MEGA{}", attr_json).into_bytes();
    let padded_len = ((attr_bytes.len() + 15) / 16) * 16;
    let mut padded = vec![0u8; padded_len];
    padded[..attr_bytes.len()].copy_from_slice(&attr_bytes);
    aes_cbc_encrypt_nopadding(&padded, key, &zero_iv)
}

// --- Chunk calculations (same as MegaBasterd) ---

pub fn calculate_chunk_offset(chunk_id: u64, size_multi: u64) -> u64 {
    let offs: [u64; 7] = [0, 128, 384, 768, 1280, 1920, 2688];
    if chunk_id <= 7 {
        offs[chunk_id as usize - 1] * 1024
    } else {
        (3584 + (chunk_id - 8) * 1024 * size_multi) * 1024
    }
}

pub fn calculate_chunk_size(chunk_id: u64, file_size: u64, offset: u64, size_multi: u64) -> u64 {
    let chunk_size = if chunk_id >= 1 && chunk_id <= 7 {
        chunk_id * 128 * 1024
    } else {
        1024 * 1024 * size_multi
    };
    if offset + chunk_size > file_size {
        file_size - offset
    } else {
        chunk_size
    }
}

pub fn gen_chunk_url(file_url: &str, file_size: u64, offset: u64, chunk_size: u64) -> String {
    if offset + chunk_size == file_size {
        format!("{}/{}", file_url, offset)
    } else {
        format!("{}/{}-{}", file_url, offset, offset + chunk_size - 1)
    }
}
