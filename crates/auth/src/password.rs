//! Passwords are stored only as Argon2id hashes. Hashing is deliberately slow (that is the point), so call
//! these from a blocking thread when inside an async server.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;

use crate::AuthError;

pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Internal)
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Does the same work as a real check against a made-up hash, so an unknown email takes as long to answer as
/// a wrong password and the response time does not reveal which emails have accounts.
pub fn spend_the_same_time(password: &str) {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    let hash =
        DUMMY.get_or_init(|| hash_password("a-fixed-throwaway-password").unwrap_or_default());
    let _ = verify_password(password, hash);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_argon2id_salted_and_verifiable() {
        let a = hash_password("correct horse battery").unwrap();
        let b = hash_password("correct horse battery").unwrap();
        assert!(a.starts_with("$argon2id$"));
        assert_ne!(a, b, "each hash has its own salt");
        assert!(!a.contains("correct horse"));
        assert!(verify_password("correct horse battery", &a));
        assert!(!verify_password("correct horse batterz", &a));
        assert!(!verify_password("anything", "not-a-hash"));
    }
}
