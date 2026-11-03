use crate::AuthError;

/// Lowercases and checks an email address (shape only; whether it works is proved by verifying it).
pub fn normalize_email(email: &str) -> Result<String, AuthError> {
    let e = email.trim().to_lowercase();
    let ok = e.len() <= 254
        && !e.contains(char::is_whitespace)
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !domain.contains("..")
        });
    if ok {
        Ok(e)
    } else {
        Err(AuthError::Invalid("enter a valid email address"))
    }
}

/// 3 to 20 characters: letters, digits and underscores. Stored lowercase, so `Magnus` and `magnus` are one name.
pub fn normalize_username(name: &str) -> Result<String, AuthError> {
    let n = name.trim().to_lowercase();
    if (3..=20).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        Ok(n)
    } else {
        Err(AuthError::Invalid(
            "username must be 3 to 20 letters, digits or underscores",
        ))
    }
}

/// Length is what protects a password, so that is the rule: 10 to 128 characters, and not just your own email or
/// username. (There are no "must contain a symbol" rules; they make passwords worse, not better.)
pub fn check_password(password: &str, email: &str, username: &str) -> Result<(), AuthError> {
    let len = password.chars().count();
    if len < 10 {
        return Err(AuthError::Invalid(
            "password must be at least 10 characters",
        ));
    }
    if len > 128 {
        return Err(AuthError::Invalid(
            "password must be at most 128 characters",
        ));
    }
    let lower = password.to_lowercase();
    if lower == email.to_lowercase()
        || lower == username.to_lowercase()
        || password
            .chars()
            .all(|c| c == password.chars().next().unwrap())
    {
        return Err(AuthError::Invalid(
            "choose a password that is not your email, your username or one repeated character",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert_eq!(
            normalize_email("  Ada@Example.COM ").unwrap(),
            "ada@example.com"
        );
        for bad in [
            "",
            "ada",
            "ada@",
            "@example.com",
            "ada@example",
            "ada@.com",
            "ada@example..com",
            "a da@example.com",
            &format!("{}@example.com", "a".repeat(250)),
        ] {
            assert!(normalize_email(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn usernames() {
        assert_eq!(normalize_username(" Magnus_1 ").unwrap(), "magnus_1");
        for bad in ["", "ab", "has space", "dash-ed", "ünïcode", &"x".repeat(21)] {
            assert!(normalize_username(bad).is_err(), "{bad:?}");
        }
        assert!(normalize_username("abc").is_ok() && normalize_username(&"x".repeat(20)).is_ok());
    }

    #[test]
    fn passwords() {
        assert!(check_password("a long enough one", "ada@example.com", "ada").is_ok());
        assert!(check_password("short", "ada@example.com", "ada").is_err());
        assert!(check_password(&"x".repeat(129), "ada@example.com", "ada").is_err());
        assert!(
            check_password("ada@example.com", "ada@example.com", "ada").is_err(),
            "not the email"
        );
        assert!(
            check_password("aaaaaaaaaaaa", "ada@example.com", "ada").is_err(),
            "not one repeated character"
        );
        assert!(
            check_password("correct horse battery staple", "ada@example.com", "ada").is_ok(),
            "long passphrases are welcome"
        );
    }
}
