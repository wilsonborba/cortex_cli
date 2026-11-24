//! Native FINAL handoff data. Credentials deliberately have no Debug formatter.

use std::{fmt, time::Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub csrf_token: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.session_id.zeroize();
        self.csrf_token.zeroize();
    }
}

impl Session {
    pub fn is_valid(&self) -> bool {
        [&self.session_id, &self.csrf_token].iter().all(|value| {
            !value.is_empty()
                && value.len() <= 4096
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    }
}

pub struct Handoff {
    pub id: String,
    pub secret: String,
    pub challenge: String,
    pub match_code: String,
    pub started: Instant,
}

impl Drop for Handoff {
    fn drop(&mut self) {
        self.id.zeroize();
        self.secret.zeroize();
    }
}

impl Handoff {
    pub fn generate() -> Result<Self, AuthFailure> {
        let mut id = [0u8; 32];
        let mut secret = [0u8; 32];
        OsRng
            .try_fill_bytes(&mut id)
            .map_err(|_| AuthFailure::Random)?;
        OsRng
            .try_fill_bytes(&mut secret)
            .map_err(|_| AuthFailure::Random)?;
        let handoff = Self::from_bytes(id, secret);
        secret.zeroize();
        Ok(handoff)
    }

    pub fn from_bytes(id: [u8; 32], mut secret: [u8; 32]) -> Self {
        let digest = Sha256::digest(secret);
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let bits =
            (u32::from(digest[0]) << 16) | (u32::from(digest[1]) << 8) | u32::from(digest[2]);
        let match_code = [19, 14, 9, 4]
            .map(|shift| char::from(alphabet[((bits >> shift) & 31) as usize]))
            .iter()
            .collect();
        let value = Self {
            id: URL_SAFE_NO_PAD.encode(id),
            secret: URL_SAFE_NO_PAD.encode(secret),
            challenge: URL_SAFE_NO_PAD.encode(digest),
            match_code,
            started: Instant::now(),
        };
        secret.zeroize();
        value
    }
}

pub struct FinishedLogin {
    pub session: Session,
    pub city: Option<String>,
    pub country: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    NotFinished,
    Expired,
    Used,
    Cancelled,
    Refused,
    Invalid,
    Disabled,
    RateLimited,
    Transport,
    Keyring,
    Random,
}

impl fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFinished => {
                "The login is not finished. Finish it in the browser, then press Enter."
            }
            Self::Expired => "This login expired. Start again.",
            Self::Used => "This login was already used. Start again.",
            Self::Cancelled => "Login cancelled. Start again.",
            Self::Refused => "Login refused. The login came from a different location.",
            Self::Invalid => "Invalid login response. Start again.",
            Self::Disabled => "Installed-app login is not available yet.",
            Self::RateLimited => "Too many login attempts. Try again later.",
            Self::Transport => "Cannot contact Asodya. Check your connection and try again.",
            Self::Keyring => "OS secure storage is unavailable. Sign in again for each session.",
            Self::Random => "Cannot prepare a secure login.",
        })
    }
}
