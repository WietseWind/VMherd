//! VNC Authentication (RFB security type 2): DES-encrypt the 16-byte challenge.

use des::Des;
use des::cipher::{Block, BlockCipherEncrypt, KeyInit};

/// Response to a VNC Authentication challenge for `password` (first 8 bytes used, zero padded,
/// each key byte bit-reversed as the protocol requires).
pub(crate) fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let cipher = Des::new(&des_key(password).into());
    let mut response = *challenge;
    // DES-ECB: both 8-byte halves are encrypted independently with the same key.
    for half in response.as_chunks_mut::<8>().0 {
        let mut block: Block<Des> = (*half).into();
        cipher.encrypt_block(&mut block);
        *half = block.into();
    }
    response
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
        assert_eq!(vnc_auth_response("secret", &challenge).to_vec(), hex("ee22539f33a5983ec12f9c2edbc995dd"));

        let challenge: [u8; 16] = hex("deadbeef00112233445566778899aabb").try_into().unwrap();
        assert_eq!(vnc_auth_response("password12", &challenge).to_vec(), hex("5aa42d801c1543995007ca855266c3b8"));
    }
}
