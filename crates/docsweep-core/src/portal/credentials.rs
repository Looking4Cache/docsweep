//! Login secrets of a portal.
//!
//! `portal.credentials` points to one keychain entry holding user name and
//! password as JSON: `{"user":"...","password":"..."}`. `portal.totp` points to
//! one holding the setup key of the second factor, in the usual Base32 form.
//! Errors in this module never quote what they could not read, and `serde_json`
//! errors are dropped on purpose because they can contain parts of the input.

use std::fmt;

use serde::{Deserialize, Serialize};
use totp_rs::{Secret, Totp};

use crate::secrets::{ExposeSecret, SecretString};

/// Why a login secret cannot be used. Never contains the secret.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("the user name must not be empty")]
    EmptyUser,
    #[error("the password must not be empty")]
    EmptyPassword,
    #[error("the keychain entry does not hold portal credentials")]
    Malformed,
    #[error("the TOTP setup key is not valid Base32")]
    InvalidTotpKey,
}

/// User name and password of a portal.
pub struct PortalCredentials {
    pub user: SecretString,
    pub password: SecretString,
}

impl fmt::Debug for PortalCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PortalCredentials(<redacted>)")
    }
}

#[derive(Serialize)]
struct StoredRef<'a> {
    user: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct Stored {
    user: String,
    password: String,
}

impl PortalCredentials {
    /// Checks both values and wraps them.
    pub fn new(user: String, password: String) -> Result<Self, CredentialError> {
        if user.trim().is_empty() {
            return Err(CredentialError::EmptyUser);
        }
        if password.is_empty() {
            return Err(CredentialError::EmptyPassword);
        }
        Ok(Self {
            user: SecretString::from(user),
            password: SecretString::from(password),
        })
    }

    /// Reads the value of a keychain entry.
    pub fn parse(stored: &SecretString) -> Result<Self, CredentialError> {
        let stored: Stored =
            serde_json::from_str(stored.expose_secret()).map_err(|_| CredentialError::Malformed)?;
        Self::new(stored.user, stored.password)
    }

    /// The value for the keychain entry.
    pub fn to_stored(&self) -> SecretString {
        let stored = StoredRef {
            user: self.user.expose_secret(),
            password: self.password.expose_secret(),
        };
        SecretString::from(
            serde_json::to_string(&stored).expect("two strings always serialize to JSON"),
        )
    }
}

/// Generates TOTP codes from a setup key.
pub struct TotpGenerator(Totp);

impl fmt::Debug for TotpGenerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TotpGenerator(<redacted>)")
    }
}

impl TotpGenerator {
    /// Portals show setup keys in groups and without padding, so both are fine.
    /// Many portals use keys shorter than the 128 bits the RFC asks for.
    pub fn new(setup_key: &SecretString) -> Result<Self, CredentialError> {
        let cleaned: String = setup_key
            .expose_secret()
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if cleaned.is_empty() {
            return Err(CredentialError::InvalidTotpKey);
        }
        let secret =
            Secret::try_from_base32(&cleaned).map_err(|_| CredentialError::InvalidTotpKey)?;
        Ok(Self(
            totp_rs::Builder::new()
                .with_secret(secret)
                .build_noncompliant(),
        ))
    }

    /// The code that is valid right now.
    pub fn current_code(&self) -> SecretString {
        SecretString::from(self.0.generate_current().to_string())
    }

    #[cfg(test)]
    fn code_at(&self, time: u64) -> String {
        self.0.generate(time).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(value: &str) -> SecretString {
        SecretString::from(value.to_owned())
    }

    #[test]
    fn credentials_round_trip_through_the_keychain_format() {
        let credentials =
            PortalCredentials::new("me@example.test".into(), "pa\"ss{w}ord".into()).unwrap();
        let parsed = PortalCredentials::parse(&credentials.to_stored()).unwrap();
        assert_eq!(parsed.user.expose_secret(), "me@example.test");
        assert_eq!(parsed.password.expose_secret(), "pa\"ss{w}ord");
    }

    #[test]
    fn the_documented_format_is_understood() {
        let parsed = PortalCredentials::parse(&secret(r#"{"user":"a","password":"b"}"#)).unwrap();
        assert_eq!(parsed.user.expose_secret(), "a");
    }

    #[test]
    fn bad_credentials_are_refused_without_quoting_them() {
        for (stored, expected) in [
            ("hunter2-secret", CredentialError::Malformed),
            (r#"{"user":"hunter2-secret"}"#, CredentialError::Malformed),
            (
                r#"{"user":7,"password":"hunter2-secret"}"#,
                CredentialError::Malformed,
            ),
            (
                r#"{"user":" ","password":"hunter2-secret"}"#,
                CredentialError::EmptyUser,
            ),
            (
                r#"{"user":"hunter2-secret","password":""}"#,
                CredentialError::EmptyPassword,
            ),
        ] {
            let error = PortalCredentials::parse(&secret(stored)).unwrap_err();
            assert_eq!(error, expected, "{stored}");
            assert!(!error.to_string().contains("hunter2"));
        }
    }

    #[test]
    fn debug_output_shows_no_secrets() {
        let credentials = PortalCredentials::new("hunter2-user".into(), "hunter2-pw".into());
        assert!(!format!("{credentials:?}").contains("hunter2"));
        let generator = TotpGenerator::new(&secret("GEZDGNBVGY3TQOJQ")).unwrap();
        assert!(!format!("{generator:?}").contains("GEZD"));
    }

    #[test]
    fn totp_codes_match_the_rfc_test_vector() {
        // RFC 6238 appendix B: the ASCII key "12345678901234567890", 59 seconds.
        let generator = TotpGenerator::new(&secret("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")).unwrap();
        assert_eq!(generator.code_at(59), "287082");
    }

    #[test]
    fn setup_keys_may_be_grouped_lowercase_and_short() {
        let plain = TotpGenerator::new(&secret("GEZDGNBVGY3TQOJQ")).unwrap();
        let grouped = TotpGenerator::new(&secret("gezd gnbv-gy3t qojq==")).unwrap();
        assert_eq!(plain.code_at(1000), grouped.code_at(1000));
        assert_eq!(plain.current_code().expose_secret().len(), 6);
    }

    #[test]
    fn invalid_setup_keys_are_refused_without_quoting_them() {
        for key in ["", "not base32 hunter2!", "1"] {
            let error = TotpGenerator::new(&secret(key)).unwrap_err();
            assert_eq!(error, CredentialError::InvalidTotpKey, "{key}");
            assert!(!error.to_string().contains("hunter2"));
        }
    }
}
