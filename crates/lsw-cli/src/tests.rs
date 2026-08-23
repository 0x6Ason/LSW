// SPDX-License-Identifier: GPL-3.0-or-later

//! CLI parsing, configuration-unit, and serialization regression tests.

use super::*;
use lsw_core::PortForward;

#[test]
fn create_accepts_repeated_tcp_publish_options() {
    let arguments = [
        "win-dev",
        "--iso",
        "windows.iso",
        "--accept-license",
        "--publish",
        "8080:80",
        "--publish",
        "8443:443",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let parsed = CreateArguments::parse(&arguments).expect("create options should parse");
    assert_eq!(
        parsed.port_forwards,
        vec![
            PortForward::new(8080, 80).expect("ports should be valid"),
            PortForward::new(8443, 443).expect("ports should be valid"),
        ]
    );
}

#[test]
fn create_rejects_malformed_tcp_publish_option() {
    let arguments = [
        "win-dev",
        "--iso",
        "windows.iso",
        "--accept-license",
        "--publish",
        "8080",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    assert!(CreateArguments::parse(&arguments).is_err());
}

#[test]
fn one_shot_install_parses_the_beta_six_beginner_flow() {
    let arguments = [
        "win-dev",
        "--iso",
        "Windows11.iso",
        "--edition",
        "pro",
        "--profile",
        "slim",
        "--no-viewer",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let parsed = InstallArguments::parse(&arguments).expect("install options should parse");
    assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
    assert_eq!(parsed.iso, Some(PathBuf::from("Windows11.iso")));
    assert_eq!(parsed.edition.as_deref(), Some("pro"));
    assert_eq!(parsed.profile, WindowsProfile::Slim);
    assert!(parsed.no_viewer);
}

#[test]
fn automatic_install_defaults_to_slim_english_and_microsoft_media() {
    let arguments = ["win-dev"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let parsed = InstallArguments::parse(&arguments).expect("install options should parse");
    assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
    assert_eq!(parsed.profile, WindowsProfile::Slim);
    assert_eq!(parsed.language, "English");
    assert!(parsed.iso.is_none());
    assert!(!parsed.language_option_seen);
    assert!(parsed.no_viewer);
}

#[test]
fn automatic_install_opens_a_viewer_only_when_requested() {
    let arguments = ["win-dev", "--viewer"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let parsed = InstallArguments::parse(&arguments).expect("install options should parse");
    assert!(!parsed.no_viewer);

    let conflicting = ["win-dev", "--viewer", "--no-viewer"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert!(InstallArguments::parse(&conflicting).is_err());
}

#[test]
fn automatic_install_accepts_an_explicit_microsoft_language() {
    let arguments = ["win-dev", "--language", "French"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let parsed = InstallArguments::parse(&arguments).expect("install options should parse");
    assert_eq!(parsed.language, "French");
    assert!(parsed.language_option_seen);
}

#[test]
fn runtime_configuration_units_are_strict_and_stable() {
    assert_eq!(parse_memory_mib("4GiB").expect("memory should parse"), 4096);
    assert_eq!(
        parse_memory_mib("4608MiB").expect("memory should parse"),
        4608
    );
    assert!(parse_memory_mib("4GB").is_err());
    assert_eq!(
        parse_duration_seconds("10m").expect("duration should parse"),
        600
    );
    assert_eq!(format_duration(600), "10m");
    assert!(parse_duration_seconds("ten minutes").is_err());
}

#[test]
fn pe_json_strings_escape_protocol_sensitive_characters() {
    let mut output = String::new();
    push_json_string(
        &mut output,
        "quote=\" slash=\\ line=\n tab=\t control=\u{0001} café 🚀",
    );
    assert_eq!(
        output,
        "\"quote=\\\" slash=\\\\ line=\\n tab=\\t control=\\u0001 café 🚀\""
    );
}
