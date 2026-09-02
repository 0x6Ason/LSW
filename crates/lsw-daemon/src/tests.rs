// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

fn supervisor() -> Supervisor {
    Supervisor::new(
        StateStore::new(std::env::temp_dir().join("lswd-dispatch-test")),
        HostCapabilities::unavailable(lsw_core::HostPlatform::Linux),
    )
}

#[test]
fn ping_protocol_is_versioned() {
    let response = dispatch("PING", &mut supervisor()).expect("ping should work");
    assert_eq!(response.len(), 5);
    assert_eq!(response[0], "PONG");
    assert_eq!(response[1], format!("PROTOCOL={DAEMON_PROTOCOL_VERSION}"));
    assert_eq!(
        response[2],
        "FEATURES=suspend,resume,hibernate,balloon,idle-exit"
    );
    assert_eq!(response[3], format!("PID={}", std::process::id()));
    assert!(response[4]
        .strip_prefix("RSS_KIB=")
        .is_some_and(|value| value.parse::<u64>().is_ok()));
}

#[test]
fn line_escaping_preserves_protocol_boundaries() {
    assert_eq!(escape_line("one\ntwo%"), "one%0Atwo%25");
}

#[test]
fn mutating_commands_are_strictly_parsed() {
    assert!(dispatch("STOP everything now", &mut supervisor()).is_err());
    assert!(dispatch("START x invalid", &mut supervisor()).is_err());
    assert!(dispatch("SUSPEND x now", &mut supervisor()).is_err());
    assert!(dispatch("RESUME", &mut supervisor()).is_err());
}
