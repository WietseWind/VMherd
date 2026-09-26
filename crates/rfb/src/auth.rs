//! VNC Authentication (RFB security type 2): DES-encrypt the 16-byte challenge.
//!
//! On macOS the DES comes from the operating system (Security.framework), so the app ships no
//! cipher implementation of its own; elsewhere the pure-Rust `des` crate does it.

use crate::Error;

/// Response to a VNC Authentication challenge for `password` (first 8 bytes used, zero padded,
/// each key byte bit-reversed as the protocol requires).
pub(crate) fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> Result<[u8; 16], Error> {
    let mut response = *challenge;
    // DES-ECB: both 8-byte halves are encrypted independently with the same key.
    #[cfg(target_os = "macos")]
    apple::encrypt_blocks(&des_key(password), &mut response)?;
    #[cfg(not(target_os = "macos"))]
    rust::encrypt_blocks(&des_key(password), &mut response);
    Ok(response)
}

/// The DES key VNC derives from a password: the first 8 bytes, zero padded, with the bit
/// order of every byte reversed (a quirk of the original implementation).
fn des_key(password: &str) -> [u8; 8] {
    let mut key = [0u8; 8];
    for (k, &p) in key.iter_mut().zip(password.as_bytes()) {
        *k = p.reverse_bits();
    }
    key
}

/// DES through Security.framework's encrypt transform.
#[cfg(target_os = "macos")]
mod apple {
    use core_foundation::data::CFData;
    use security_framework::key::{KeyType, SecKey};
    use security_framework::os::macos::encrypt_transform::{Builder, Mode, Padding};
    // Apple deprecated `SecKeyCreateFromData` without a replacement for symmetric keys; it is
    // still the only safe route to the system's DES.
    #[allow(deprecated)]
    use security_framework::os::macos::key::SecKeyExt;

    use crate::Error;

    /// Encrypts each 8-byte block on its own (one block per transform, so the result is ECB
    /// whatever chaining mode the transform would pick for longer input).
    pub(super) fn encrypt_blocks(key: &[u8; 8], blocks: &mut [u8; 16]) -> Result<(), Error> {
        let failed = |e: &dyn std::fmt::Display| Error::AuthFailed(format!("DES (Security.framework): {e}"));
        #[allow(deprecated)]
        let key = SecKey::from_data(KeyType::des(), &CFData::from_buffer(key)).map_err(|e| failed(&e))?;
        for block in blocks.as_chunks_mut::<8>().0 {
            let encrypted = Builder::new()
                .padding(Padding::none())
                .mode(Mode::ecb())
                .iv(CFData::from_buffer(&[0; 8]))
                .encrypt(&key, &CFData::from_buffer(block))
                .map_err(|e| failed(&e))?;
            *block = encrypted.bytes().try_into().map_err(|_| failed(&"unexpected output length"))?;
        }
        Ok(())
    }
}

/// DES from the `des` crate (also built for the macOS tests, to cross-check the system's).
#[cfg(any(not(target_os = "macos"), test))]
mod rust {
    use des::Des;
    use des::cipher::{Block, BlockCipherEncrypt, KeyInit};

    pub(super) fn encrypt_blocks(key: &[u8; 8], blocks: &mut [u8; 16]) {
        let cipher = Des::new(&(*key).into());
        for half in blocks.as_chunks_mut::<8>().0 {
            let mut block: Block<Des> = (*half).into();
            cipher.encrypt_block(&mut block);
            *half = block.into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn key_is_bit_reversed_and_padded() {
        // 's' = 0b0111_0011 -> 0b1100_1110 = 0xce, ...
        assert_eq!(des_key("secret").to_vec(), hex("cea6c64ea62e0000"));
        assert_eq!(des_key(""), [0; 8]);
        // Only the first 8 bytes count.
        assert_eq!(des_key("password12"), des_key("password"));
    }

    // Vectors computed independently of this code with
    //   printf '<challenge bytes>' | openssl enc -des-ecb -K <bit-reversed key hex> -nopad | xxd -p
    // (macOS LibreSSL 3.3.6 and OpenSSL 3.6 with the legacy provider agree).
    #[test]
    fn response_matches_openssl_vectors() {
        let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(vnc_auth_response("secret", &challenge).unwrap().to_vec(), hex("ee22539f33a5983ec12f9c2edbc995dd"));

        let challenge: [u8; 16] = hex("deadbeef00112233445566778899aabb").try_into().unwrap();
        assert_eq!(
            vnc_auth_response("password12", &challenge).unwrap().to_vec(),
            hex("5aa42d801c1543995007ca855266c3b8")
        );
    }

    /// Classic DES known-answer vectors (key, plaintext, ciphertext), for each implementation.
    const DES_VECTORS: [(&str, &str, &str); 3] = [
        ("133457799bbcdff1", "0123456789abcdef", "85e813540f0ab405"),
        ("0000000000000000", "0000000000000000", "8ca64de9c1b123a7"),
        ("ffffffffffffffff", "ffffffffffffffff", "7359b2163e4edc58"),
    ];

    fn check_known_answers(encrypt: impl Fn(&[u8; 8], &mut [u8; 16])) {
        for (key, plain, cipher) in DES_VECTORS {
            let key: [u8; 8] = hex(key).try_into().unwrap();
            // The same block twice: ECB must give the same ciphertext twice.
            let mut blocks: [u8; 16] = hex(&plain.repeat(2)).try_into().unwrap();
            encrypt(&key, &mut blocks);
            assert_eq!(blocks.to_vec(), hex(&cipher.repeat(2)), "key {key:02x?}");
        }
    }

    #[test]
    fn rust_des_known_answers() {
        check_known_answers(rust::encrypt_blocks);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn system_des_known_answers_and_agreement() {
        check_known_answers(|key, blocks| apple::encrypt_blocks(key, blocks).unwrap());
        for password in ["", "secret", "password12", "\u{e9}\u{e8}xyz!", "12345678"] {
            let challenge: [u8; 16] = std::array::from_fn(|i| (i as u8).wrapping_mul(37) ^ 0x5a);
            let mut expected = challenge;
            rust::encrypt_blocks(&des_key(password), &mut expected);
            assert_eq!(vnc_auth_response(password, &challenge).unwrap(), expected, "{password:?}");
        }
    }
}
