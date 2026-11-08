//! Password hashing.
//!
//! New hashes are Argon2id PHC strings. Accounts imported from 2.x carry Better Auth
//! hashes, `<salt>:<key>` in hex, produced by scrypt with N=16384, r=16, p=1 and a
//! 64-byte key, where the 32-character hex salt *string* (its UTF-8 bytes) is the
//! salt. Both forms verify; a legacy hash is replaced on the next successful sign-in.
//! Passwords are NFKC-normalized first, as Better Auth did.

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use subtle::ConstantTimeEq;
use unicode_normalization::UnicodeNormalization;

pub const MIN_LENGTH: usize = 8;
pub const MAX_LENGTH: usize = 128;

/// A valid Argon2id hash of a random password, verified against when an email has
/// no account so that "no such user" and "wrong password" take similar time.
const DECOY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$YUsi4a8d8Pz9DPWQ5lsGztNtX5Q6oVUlWmvFSgLA2t8";

#[derive(Debug, PartialEq, Eq)]
pub enum Verification {
    Invalid,
    /// Correct; `rehash` is set when the stored hash uses the legacy scheme.
    Valid {
        rehash: bool,
    },
}

pub fn length_is_valid(password: &str) -> bool {
    (MIN_LENGTH..=MAX_LENGTH).contains(&password.chars().count())
}

fn normalized(password: &str) -> String {
    password.nfkc().collect()
}

pub fn hash(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(normalized(password).as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("hash password: {error}"))
}

pub fn verify(stored: &str, password: &str) -> Verification {
    if stored.starts_with("$argon2") {
        let Ok(parsed) = PasswordHash::new(stored) else {
            return Verification::Invalid;
        };
        return if Argon2::default()
            .verify_password(normalized(password).as_bytes(), &parsed)
            .is_ok()
        {
            Verification::Valid { rehash: false }
        } else {
            Verification::Invalid
        };
    }
    match stored.split_once(':') {
        Some((salt, key)) if verify_better_auth_scrypt(salt, key, password) => {
            Verification::Valid { rehash: true }
        }
        _ => Verification::Invalid,
    }
}

fn verify_better_auth_scrypt(salt: &str, key_hex: &str, password: &str) -> bool {
    let Ok(expected) = hex::decode(key_hex) else {
        return false;
    };
    if expected.len() != 64 || salt.is_empty() {
        return false;
    }
    // log2(16384) = 14.
    let Ok(params) = scrypt::Params::new(14, 16, 1, 64) else {
        return false;
    };
    let mut derived = [0_u8; 64];
    if scrypt::scrypt(
        normalized(password).as_bytes(),
        salt.as_bytes(),
        &params,
        &mut derived,
    )
    .is_err()
    {
        return false;
    }
    derived.ct_eq(&expected).into()
}

/// Hashing costs tens of milliseconds and ~19–32 MiB; keep it off the async workers.
pub async fn hash_blocking(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || hash(&password)).await?
}

pub async fn verify_blocking(stored: Option<String>, password: String) -> Verification {
    tokio::task::spawn_blocking(move || match stored {
        Some(stored) => verify(&stored, &password),
        None => {
            let _ = verify(DECOY_HASH, &password);
            Verification::Invalid
        }
    })
    .await
    .unwrap_or(Verification::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by Better Auth 1.6.23's own hashPassword (@better-auth/utils, node:crypto).
    const BETTER_AUTH_HASH: &str = "06f2dfcd5e94507bfc19b341cbe98a90:f5a0b7f0eb93538e3b751f3516fa8215b6057c61ea3ca7d1909cf7b73711d646deb1c354dc74b3a397c4ed0cd35b6932a1d19fdca6e854a6f43a35b1667e3836";
    const BETTER_AUTH_UNICODE_HASH: &str = "c2fc84615f31d0ee0a8652255188fb9b:4ce9ef8480a0417e1886b5910da8c6e5b0a537a30bc93d144193ca58597d11f184df54e78cd619c7df1190dd47dce6356dca5083105acdb2a5de078df9d69b12";

    #[test]
    fn better_auth_scrypt_hashes_verify_and_ask_for_a_rehash() {
        assert_eq!(
            verify(BETTER_AUTH_HASH, "correct horse battery"),
            Verification::Valid { rehash: true }
        );
        assert_eq!(
            verify(BETTER_AUTH_HASH, "correct horse batterY"),
            Verification::Invalid
        );
    }

    #[test]
    fn legacy_verification_normalizes_like_better_auth() {
        assert_eq!(
            verify(BETTER_AUTH_UNICODE_HASH, "Ŝtraße ﬁx 🔑 ქართული"),
            Verification::Valid { rehash: true }
        );
        // NFKC folds the ligature, so the decomposed spelling verifies too.
        assert_eq!(
            verify(BETTER_AUTH_UNICODE_HASH, "Ŝtraße fix 🔑 ქართული"),
            Verification::Valid { rehash: true }
        );
    }

    #[test]
    fn new_hashes_are_argon2id_and_need_no_rehash() {
        let stored = hash("a long enough password").unwrap();
        assert!(stored.starts_with("$argon2id$"));
        assert_eq!(
            verify(&stored, "a long enough password"),
            Verification::Valid { rehash: false }
        );
        assert_eq!(
            verify(&stored, "a long enough passworD"),
            Verification::Invalid
        );
    }

    #[test]
    fn malformed_hashes_never_verify() {
        for stored in ["", ":", "abc", "zz:zz", "$argon2id$broken", "salt:00"] {
            assert_eq!(
                verify(stored, "anything"),
                Verification::Invalid,
                "{stored}"
            );
        }
    }

    #[test]
    fn the_decoy_hash_is_well_formed() {
        assert!(PasswordHash::new(DECOY_HASH).is_ok());
    }

    #[test]
    fn length_limits_match_the_sign_up_form() {
        assert!(!length_is_valid("short"));
        assert!(length_is_valid("12345678"));
        assert!(length_is_valid(&"x".repeat(128)));
        assert!(!length_is_valid(&"x".repeat(129)));
    }
}
