// SPDX-License-Identifier: GPL-3.0-or-later

//! The host-guest contract that is not a wire type: the loopback ports the
//! guest services listen on, the file names of the boot identity volume, and
//! the derivation of scoped credentials from the agent token.

use sha2::{Digest, Sha256};

use crate::{ProtocolError, Result};

// Windows reserves TCP 5040 for the Connected Devices Platform service on
// clean installations. Keep LSW outside both that system-service collision
// and Windows' default dynamic client-port range (49152-65535).
pub const AGENT_GUEST_PORT: u16 = 35040;
pub const LICENSE_HELPER_GUEST_PORT: u16 = 35041;
pub const USER_HELPER_GUEST_PORT: u16 = 35042;
pub const MAINTENANCE_HELPER_GUEST_PORT: u16 = 35043;
pub const DESKTOP_COMPANION_GUEST_PORT: u16 = 35044;

/// Files on the read-only boot identity volume consumed by the guest agent.
pub const CLONE_IDENTITY_MARKER_FILE: &str = "clone-identity.marker";
pub const CLONE_IDENTITY_NAME_FILE: &str = "instance.name";
pub const CLONE_IDENTITY_TOKEN_FILE: &str = "agent.token";

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
        return Err(ProtocolError::Protocol(
            "credential scope must contain 1-64 lowercase ASCII letters, digits, or hyphens"
                .to_owned(),
        ));
    }

    let mut hash = Sha256::new();
    hash.update(b"LSW scoped credential v1\0");
    hash.update(scope.as_bytes());
    hash.update(b"\0");
    hash.update(agent_token.as_bytes());
    Ok(hex_encode(&hash.finalize()))
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ProtocolError::Protocol(
            "agent token must contain 64 lowercase hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn scoped_credentials_are_stable_separate_and_do_not_reveal_the_token() {
        let desktop = derive_scoped_credential(TOKEN, DESKTOP_COMPANION_CREDENTIAL_SCOPE)
            .expect("desktop scope is valid");
        let live = derive_scoped_credential(TOKEN, LIVE_SHARE_CREDENTIAL_SCOPE)
            .expect("live share scope is valid");
        assert_eq!(desktop.len(), 64);
        assert_eq!(live.len(), 64);
        assert_ne!(desktop, live);
        assert_ne!(desktop, TOKEN);
        assert_ne!(live, TOKEN);
        assert_eq!(
            derive_scoped_credential(TOKEN, DESKTOP_COMPANION_CREDENTIAL_SCOPE)
                .expect("derivation is deterministic"),
            desktop
        );
        assert!(desktop.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn scoped_credentials_reject_malformed_tokens_and_scopes() {
        assert!(derive_scoped_credential("short", LIVE_SHARE_CREDENTIAL_SCOPE).is_err());
        assert!(derive_scoped_credential(&TOKEN.to_ascii_uppercase(), "live-share-v1").is_err());
        assert!(derive_scoped_credential(TOKEN, "").is_err());
        assert!(derive_scoped_credential(TOKEN, "Live Share").is_err());
        assert!(derive_scoped_credential(TOKEN, &"a".repeat(65)).is_err());
    }

    #[test]
    fn guest_ports_are_distinct_and_outside_the_windows_dynamic_range() {
        let ports = [
            AGENT_GUEST_PORT,
            LICENSE_HELPER_GUEST_PORT,
            USER_HELPER_GUEST_PORT,
            MAINTENANCE_HELPER_GUEST_PORT,
            DESKTOP_COMPANION_GUEST_PORT,
        ];
        for (index, port) in ports.iter().enumerate() {
            assert!(*port < 49152 && *port != 5040);
            assert!(!ports[index + 1..].contains(port));
        }
    }
}
