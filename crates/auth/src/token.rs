//! Access and refresh tokens are 256 random bits. The player holds the token; the server stores only its
//! SHA-256, so a leaked database cannot be replayed as sign-ins.

use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_unique_and_hash_stably() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert_ne!(hash_token(&a), a);
        assert_eq!(hash_token(&a).len(), 64);
    }
}
