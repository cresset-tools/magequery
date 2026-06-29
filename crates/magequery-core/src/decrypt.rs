//! Decryption of Magento-encrypted config values, mirroring
//! `Magento\Framework\Encryption\Encryptor`. We never execute PHP.
//!
//! An encrypted value is `keyVersion:cipher[:iv]:base64(payload)`, with shorthands:
//! - `kv:cipher:data`   (the common modern form)
//! - `cipher:data`      (keyVersion = 0)
//! - `data`             (keyVersion = 0, cipher = Blowfish)
//! - `kv:_:iv:data`     (4-part legacy → cipher is forced to Rijndael-256)
//!
//! Cipher versions: 0 = Blowfish, 1 = Rijndael-128/ECB (= AES, ECB), 2 = Rijndael-256/CBC,
//! 3 = ChaCha20-Poly1305 IETF (the modern default). The key for `keyVersion` is used
//! directly (Magento does no further key derivation); we try **that** key with the value's
//! own cipher.
//!
//! Cipher 2 (Rijndael-256 — a 256-bit *block*, not AES) mirrors Magento's mcrypt `Mcrypt`
//! adapter: `MCRYPT_RIJNDAEL_256` in CBC mode, 32-byte key, 32-byte IV, mcrypt zero-padding.
//! The IV is the 3rd field of the 4-part `keyVersion:cipher:iv:data` form; we accept it
//! base64-encoded (the robust on-disk encoding) or as raw 32 bytes, and fall back to a
//! zero IV when absent (mcrypt's default).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::BlockDecrypt;
use base64::Engine;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use simple_rijndael::impls::RijndaelCbc;
use simple_rijndael::paddings::ZeroPadding;

/// Holds the crypt key(s) from `env.php`. Obtained from
/// [`Magento::decryptor`](crate::Magento::decryptor).
pub struct Decryptor {
    keys: Vec<String>,
}

impl Decryptor {
    pub(crate) fn new(keys: Vec<String>) -> Self {
        Self { keys }
    }

    /// Whether a value looks like a Magento-encrypted blob (`<digits>:<digits>:…`).
    pub fn is_encrypted(value: &str) -> bool {
        let mut it = value.splitn(3, ':');
        matches!(
            (it.next(), it.next(), it.next()),
            (Some(a), Some(b), Some(c))
                if !a.is_empty() && a.bytes().all(|x| x.is_ascii_digit())
                    && !b.is_empty() && b.bytes().all(|x| x.is_ascii_digit())
                    && !c.is_empty()
        )
    }

    /// The cipher version of an encrypted value, if parseable.
    pub fn cipher_version(value: &str) -> Option<u32> {
        parse(value).map(|(_, cipher, _, _)| cipher)
    }

    /// Decrypt an encrypted value, or `None` if it isn't encrypted / the key is missing /
    /// the cipher is unsupported (legacy Blowfish) / authentication fails.
    /// The result is trimmed, as Magento's `Encryptor::decrypt` does.
    pub fn decrypt(&self, value: &str) -> Option<String> {
        let (key_version, cipher, iv, data_b64) = parse(value)?;
        let key = self.keys.get(key_version)?.as_bytes();
        let data = base64::engine::general_purpose::STANDARD.decode(data_b64.trim()).ok()?;

        let plain = match cipher {
            3 => decrypt_chacha(key, &data),
            2 => decrypt_rijndael256_cbc(key, &iv_bytes(iv), &data),
            1 => decrypt_aes_ecb(key, &data),
            // 0 (Blowfish) — legacy mcrypt, no Rust impl; unsupported.
            _ => None,
        }?;
        Some(plain.trim().to_string())
    }
}

/// The 32-byte IV for Rijndael-256/CBC, from the value's `iv` field: base64 if it decodes to
/// 32 bytes, else the raw bytes if already 32, else a zero IV (mcrypt's default when absent).
fn iv_bytes(iv: Option<&str>) -> [u8; 32] {
    if let Some(iv) = iv {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(iv.trim()) {
            if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return arr;
            }
        }
        if let Ok(arr) = <[u8; 32]>::try_from(iv.as_bytes()) {
            return arr;
        }
    }
    [0u8; 32]
}

/// `(keyVersion, cipher, iv, base64data)`.
fn parse(value: &str) -> Option<(usize, u32, Option<&str>, &str)> {
    let parts: Vec<&str> = value.splitn(4, ':').collect();
    match parts.as_slice() {
        // 4-part legacy: keyVersion : (ignored) : iv : data — cipher forced to Rijndael-256.
        [kv, _cv, iv, data] => Some((kv.parse().ok()?, 2, Some(iv), data)),
        [kv, cv, data] => Some((kv.parse().ok()?, cv.parse().ok()?, None, data)),
        [cv, data] => Some((0, cv.parse().ok()?, None, data)),
        [data] => Some((0, 0, None, data)),
        _ => None,
    }
}

fn decrypt_chacha(key: &[u8], data: &[u8]) -> Option<String> {
    // 12-byte nonce ‖ ciphertext ‖ 16-byte Poly1305 tag.
    if key.len() < 32 || data.len() < 12 + 16 {
        return None;
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key[..32]));
    let nonce = Nonce::from_slice(&data[..12]);
    let plaintext = cipher.decrypt(nonce, &data[12..]).ok()?;
    Some(String::from_utf8_lossy(&plaintext).into_owned())
}

/// Cipher version 2: mcrypt `RIJNDAEL_256` (a 256-bit *block* — not AES) in CBC mode,
/// zero-padded. 32-byte key, 32-byte block/IV.
fn decrypt_rijndael256_cbc(key: &[u8], iv: &[u8; 32], data: &[u8]) -> Option<String> {
    if key.len() < 32 || data.is_empty() || data.len() % 32 != 0 {
        return None;
    }
    let cbc = RijndaelCbc::<ZeroPadding>::new(&key[..32], 32).ok()?;
    let plain = cbc.decrypt(iv, data.to_vec()).ok()?;
    // ZeroPadding::decode in the crate is effectively a no-op; strip mcrypt's zero padding.
    let s = String::from_utf8_lossy(&plain);
    Some(s.trim_end_matches('\0').to_string())
}

/// Cipher version 1: mcrypt `RIJNDAEL_128` (= AES, 128-bit block) in ECB mode, zero-padded.
/// Magento's 32-byte key selects AES-256.
fn decrypt_aes_ecb(key: &[u8], data: &[u8]) -> Option<String> {
    if key.len() < 32 || data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = aes::Aes256::new(GenericArray::from_slice(&key[..32]));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = *GenericArray::from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    // mcrypt zero-pads; strip trailing NULs before the caller trims whitespace.
    let s = String::from_utf8_lossy(&out);
    Some(s.trim_end_matches('\0').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    const K0: &str = "0123456789abcdef0123456789abcdef"; // 32 bytes
    const K1: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"; // 32 bytes

    fn enc_chacha(key: &str, plain: &[u8]) -> String {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key.as_bytes()));
        let nonce = Nonce::from_slice(b"unique-nonce");
        let ct = cipher.encrypt(nonce, plain).unwrap();
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        STANDARD.encode(blob)
    }

    #[test]
    fn chacha_v3_roundtrip_and_keyversion() {
        let d = Decryptor::new(vec![K0.to_string(), K1.to_string()]);
        let v0 = format!("0:3:{}", enc_chacha(K0, b"secret-zero"));
        let v1 = format!("1:3:{}", enc_chacha(K1, b"secret-one"));
        assert_eq!(d.decrypt(&v0).as_deref(), Some("secret-zero"));
        assert_eq!(d.decrypt(&v1).as_deref(), Some("secret-one"));
        // Wrong key version → fails (auth).
        let wrong = format!("1:3:{}", enc_chacha(K0, b"x"));
        assert_eq!(d.decrypt(&wrong), None);
    }

    #[test]
    fn aes_ecb_v1_roundtrip() {
        use aes::cipher::{BlockEncrypt, KeyInit};
        let d = Decryptor::new(vec![K0.to_string()]);
        let cipher = aes::Aes256::new(GenericArray::from_slice(K0.as_bytes()));
        let mut padded = b"hello-world".to_vec();
        padded.resize(16, 0); // mcrypt zero-padding to the block size
        let mut block = *GenericArray::from_slice(&padded);
        cipher.encrypt_block(&mut block);
        let value = format!("0:1:{}", STANDARD.encode(block));
        assert_eq!(d.decrypt(&value).as_deref(), Some("hello-world"));
    }

    #[test]
    fn rijndael256_v2_roundtrip() {
        let d = Decryptor::new(vec![K0.to_string()]);
        let iv = [7u8; 32];
        let cbc = RijndaelCbc::<ZeroPadding>::new(K0.as_bytes(), 32).unwrap();
        let ct = cbc.encrypt(&iv, b"rijndael-secret".to_vec()).unwrap();
        // 4-part form: keyVersion:cipher:iv:data (iv + data base64-encoded).
        let value = format!("0:2:{}:{}", STANDARD.encode(iv), STANDARD.encode(&ct));
        assert_eq!(d.decrypt(&value).as_deref(), Some("rijndael-secret"));
        // Wrong IV → garbage out (the first block is corrupted), not the plaintext.
        let bad_iv = format!("0:2:{}:{}", STANDARD.encode([9u8; 32]), STANDARD.encode(&ct));
        assert_ne!(d.decrypt(&bad_iv).as_deref(), Some("rijndael-secret"));
    }

    #[test]
    fn legacy_ciphers_and_plain() {
        let d = Decryptor::new(vec![K0.to_string()]);
        assert_eq!(d.decrypt("0:2:abc"), None); // cipher 2, but no valid block data
        assert_eq!(d.decrypt("0:0:abc"), None); // Blowfish unsupported
        assert!(!Decryptor::is_encrypted("just a plain value"));
        assert!(!Decryptor::is_encrypted("12:30")); // not encrypted (only 2 parts)
        assert_eq!(Decryptor::cipher_version("0:3:xx"), Some(3));
    }
}
