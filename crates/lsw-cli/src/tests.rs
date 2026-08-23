// SPDX-License-Identifier: GPL-3.0-or-later

//! CLI parsing, configuration-unit, and serialization regression tests.

use super::*;
use lsw_core::PortForward;

#[test]
fn agent_wait_covers_slow_first_windows_cold_boots() {
    assert!(AGENT_START_TIMEOUT >= Duration::from_secs(3 * 60));
}

#[test]
fn create_accepts_repeated_tcp_publish_options() {
    let arguments = [
        "win-dev",
        "--iso",
        "windows.iso",
        "--accept-windows-license",
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
        resolve_port_forwards(&parsed.port_forwards, &parsed.name).expect("ports should resolve"),
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
fn create_allocates_dynamic_loopback_ports() {
    let arguments = [
        "win-dev",
        "--iso",
        "windows.iso",
        "--accept-license",
        "--publish",
        "auto:8080",
        "--publish",
        "0:8443",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let parsed = CreateArguments::parse(&arguments).expect("dynamic ports should parse");
    let forwards =
        resolve_port_forwards(&parsed.port_forwards, &parsed.name).expect("ports should resolve");
    assert_eq!(forwards.len(), 2);
    assert_eq!(forwards[0].guest_port, 8080);
    assert_eq!(forwards[1].guest_port, 8443);
    assert_ne!(forwards[0].host_port, 0);
    assert_ne!(forwards[0].host_port, forwards[1].host_port);
    assert_ne!(
        forwards[0].host_port,
        lsw_core::control_port_for_instance(&parsed.name).unwrap()
    );
}

#[test]
fn guest_exec_parses_cwd_and_repeated_environment() {
    let arguments = [
        "win-dev",
        "--cwd=C:\\work tree",
        "-e",
        "MODE=release",
        "--env=EMPTY=",
        "--",
        "cmd.exe",
        "/C",
        "exit 23",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    let parsed = GuestCommandArguments::parse(&arguments, SessionKind::Exec)
        .expect("exec arguments should parse");
    assert_eq!(parsed.requested.as_deref(), Some("win-dev"));
    assert_eq!(
        parsed.request.working_directory.as_deref(),
        Some("C:\\work tree")
    );
    assert_eq!(parsed.request.argv, vec!["cmd.exe", "/C", "exit 23"]);
    assert_eq!(
        parsed.environment.variables,
        vec![
            ("MODE".to_owned(), "release".to_owned()),
            ("EMPTY".to_owned(), String::new()),
        ]
    );
    assert!(!parsed.detached);
}

#[test]
fn detach_is_run_only_and_environment_names_are_case_insensitive() {
    let detached = ["--detach", "--", "worker.exe"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert!(
        GuestCommandArguments::parse(&detached, SessionKind::Run)
            .expect("run detach should parse")
            .detached
    );
    assert!(GuestCommandArguments::parse(&detached, SessionKind::Exec).is_err());

    let duplicate_environment = ["-e", "Path=one", "-e", "PATH=two", "--", "cmd.exe"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert!(GuestCommandArguments::parse(&duplicate_environment, SessionKind::Exec).is_err());
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
        "--accept-windows-license",
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
    assert!(parsed.accept_windows_license);
}

#[test]
fn windows_license_acceptance_has_a_clear_option_and_compatibility_alias() {
    for option in ["--accept-windows-license", "--accept-license"] {
        let arguments = ["win-dev", option]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
        let parsed = InstallArguments::parse(&arguments).expect("license option should parse");
        assert!(parsed.accept_windows_license);
    }

    let duplicate = ["win-dev", "--accept-windows-license", "--accept-license"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    assert!(InstallArguments::parse(&duplicate).is_err());

    for accepted in ["y", "Y", "yes", "YES", " yes "] {
        assert!(installation::affirmative_license_response(accepted));
    }
    for rejected in ["", "n", "no", "true", "1"] {
        assert!(!installation::affirmative_license_response(rejected));
    }
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
    assert!(!parsed.accept_windows_license);
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
