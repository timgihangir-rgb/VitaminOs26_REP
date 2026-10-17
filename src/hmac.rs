// src/hmac.rs
//
// HMAC-SHA256 (RFC 2104).
//
//   HMAC(K, m) = H((K' XOR opad) || H((K' XOR ipad) || m))
// где K' = K, дополненный нулями до BLOCK = 64, либо H(K), если K длиннее.

use crate::sha256;

const BLOCK: usize = 64;

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        // Длинный ключ сжимаем до 32 байт.
        let h = sha256::hash(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
        // Остальные байты k уже 0.
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5Cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    // inner = SHA256(ipad || msg)
    let mut inner = sha256::Sha256State::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_h = inner.finalize();

    // outer = SHA256(opad || inner)
    let mut outer = sha256::Sha256State::new();
    outer.update(&opad);
    outer.update(&inner_h);
    outer.finalize()
}
