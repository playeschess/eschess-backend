//! The identity rules that need no database: hashing passwords, making and hashing tokens, and checking
//! what players type. Nothing here stores anything.

pub mod password;
pub mod token;
pub mod validate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Something a player typed was not acceptable; the text says what to fix.
    Invalid(&'static str),
    /// Hashing failed (not the player's fault).
    Internal,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Invalid(why) => write!(f, "{why}"),
            AuthError::Internal => write!(f, "internal error"),
        }
    }
}

impl std::error::Error for AuthError {}
