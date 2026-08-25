// SPDX-License-Identifier: GPL-3.0-or-later

use sha2::{Digest, Sha256};

use crate::{LswError, Result};

pub const DESKTOP_COMPANION_CREDENTIAL_SCOPE: &str = "desktop-companion-v1";
pub const LIVE_SHARE_CREDENTIAL_SCOPE: &str = "live-share-v1";

/// Derives a domain-separated credential without exposing the agent token to
/// less-privileged guest integrations.
pub fn derive_scoped_credential(agent_token: &str, scope: &str) -> Result<String> {
    validate_token(agent_token)?;
    if scope.is_empty()
        || scope.len() > 64
        || !scope
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(LswError::InvalidValue {
            field: "credential scope",
            reason: "must contain 1-64 lowercase ASCII letters, digits, or hyphens".to_owned(),
        });
    }

    let mut hash = Sha256::new();
    hash.update(b"LSW scoped credential v1\0");
    hash.update(scope.as_bytes());
    hash.update(b"\0");
    hash.update(agent_token.as_bytes());
    let digest = hash.finalize();
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(output)
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(LswError::InvalidValue {
            field: "agent token",
            reason: "must contain 64 lowercase hexadecimal characters".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_credentials_are_stable_separate_and_do_not_reveal_the_token() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let desktop = derive_scoped_credential(token, DESKTOP_COMPANION_CREDENTIAL_SCOPE).unwrap();
        let live = derive_scoped_credential(token, LIVE_SHARE_CREDENTIAL_SCOPE).unwrap();
        assert_eq!(desktop.len(), 64);
        assert_eq!(live.len(), 64);
        assert_ne!(desktop, live);
        assert_ne!(desktop, token);
        assert_eq!(
            desktop,
            derive_scoped_credential(token, DESKTOP_COMPANION_CREDENTIAL_SCOPE).unwrap()
        );
    }

    #[test]
    fn malformed_tokens_and_scopes_are_rejected() {
        assert!(derive_scoped_credential("secret", LIVE_SHARE_CREDENTIAL_SCOPE).is_err());
        let token = "a".repeat(64);
        assert!(derive_scoped_credential(&token, "Desktop Companion").is_err());
        assert!(derive_scoped_credential(&token, "").is_err());
    }
}
