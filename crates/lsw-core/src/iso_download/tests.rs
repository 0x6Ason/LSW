// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::downloader::*;
use super::*;

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

fn fixture() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "lsw-iso-download-test-{}-{nonce}-{id}",
        std::process::id()
    ));
    fs::create_dir(&root).expect("fixture should be created");
    root
}

#[test]
fn parses_current_product_and_official_sha_table() {
    let html = r#"
        <select id="product-edition">
          <option value="">Select edition</option>
          <option value="3321">Windows 11 multi-edition</option>
        </select>
        <table><tbody>
          <tr><th>Country Locale</th><th>Hash Code</th></tr>
          <tr><td>English 64-bit</td><td>768984706B909479417B2368438909440F2967FF05C6A9195ED2667254E465E3</td></tr>
          <tr><td>French 64-bit</td><td>A02693BEB8EB166AFDFDB7DB49176A2B547F81E61030A695FE172277DB6A1977</td></tr>
        </tbody></table>
    "#;
    let catalog = parse_download_page(html).expect("download page should parse");
    assert_eq!(catalog.product_id, "3321");
    assert_eq!(
        catalog
            .hash_for("English", "x64")
            .expect("English hash should exist"),
        "768984706B909479417B2368438909440F2967FF05C6A9195ED2667254E465E3"
    );
    assert!(catalog.hash_for("German", "x64").is_err());
}

#[test]
fn parses_microsoft_language_and_download_payloads() {
    let languages = parse_languages(
        r#"{"Skus":[{"Id":"123","Language":"English"},{"Id":"456","Language":"French"}]}"#,
    )
    .expect("language response should parse");
    assert_eq!(
        select_language(&languages, "english")
            .expect("language should match")
            .id,
        "123"
    );

    let response = parse_download_response(
        r#"{"ProductDownloadOptions":[{"Uri":"https://software.download.prss.microsoft.com/path/windows.iso?t=secret","DownloadType":1}],"DownloadExpirationDatetime":"2026-08-18T00:00:00Z"}"#,
    )
    .expect("download response should parse");
    assert_eq!(response.options[0].architecture(), "x64");
    assert_eq!(response.expiration.as_deref(), Some("2026-08-18T00:00:00Z"));
}

#[test]
fn signed_urls_are_allowlisted_and_redacted() {
    let url = SecretDownloadUrl::parse(
        "https://software.download.prss.microsoft.com/path/windows.iso?t=secret&P1=token",
    )
    .expect("Microsoft CDN URL should be accepted");
    let debug = format!("{url:?}");
    assert!(debug.contains("software.download.prss.microsoft.com/path/windows.iso"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("token"));
    assert!(SecretDownloadUrl::parse(
        "http://software.download.prss.microsoft.com/path/windows.iso?t=secret"
    )
    .is_err());
    assert!(SecretDownloadUrl::parse(
        "https://software.download.prss.microsoft.com.evil.example/windows.iso?t=secret"
    )
    .is_err());
}

#[test]
fn native_ranges_use_no_more_than_four_connections_and_cover_every_byte() {
    assert_eq!(
        split_ranges(10).expect("ranges should split"),
        vec![
            ByteRange { start: 0, end: 2 },
            ByteRange { start: 3, end: 5 },
            ByteRange { start: 6, end: 8 },
            ByteRange { start: 9, end: 9 },
        ]
    );
    assert_eq!(split_ranges(3).expect("ranges should split").len(), 3);
    assert!(split_ranges(0).is_err());
}

#[test]
fn sha256_verification_is_exact_and_existing_mismatches_fail_closed() {
    let root = fixture();
    let iso = root.join("windows.iso");
    fs::write(&iso, b"abc").expect("fixture should be written");
    assert_eq!(
        sha256_file(&iso).expect("hash should compute"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let resolved = ResolvedWindowsIso {
        product_id: "3321".to_owned(),
        sku_id: "123".to_owned(),
        language: "English".to_owned(),
        architecture: "x64".to_owned(),
        filename: "windows.iso".to_owned(),
        expected_sha256: "0".repeat(64),
        expires_at: None,
        download_url: SecretDownloadUrl::parse(
            "https://software.download.prss.microsoft.com/path/windows.iso?t=secret",
        )
        .expect("URL should parse"),
    };
    assert!(
        existing_verified_iso(&iso, &resolved, IsoDownloadEngine::Native, &mut |_| {}).is_err()
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
fn completed_download_never_overwrites_a_destination_that_appeared() {
    let root = fixture();
    let temporary = root.join(".windows.iso.lsw-download");
    let destination = root.join("windows.iso");
    fs::write(&temporary, b"verified download").expect("temporary should be written");
    fs::write(&destination, b"unrelated file").expect("destination should be written");

    let error = promote_download(&temporary, &destination)
        .expect_err("existing destination must not be overwritten");
    assert!(error.to_string().contains("refusing to overwrite"));
    assert_eq!(
        fs::read(&destination).expect("destination should remain readable"),
        b"unrelated file"
    );
    assert!(temporary.exists());
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
fn sha256_reports_exact_bounded_progress() {
    let root = fixture();
    let input = root.join("input.iso");
    fs::write(&input, vec![0x5a; 1024 * 1024 + 17]).expect("fixture should be written");
    let mut events = Vec::new();
    let digest = sha256_file_with_progress(&input, &mut |event| events.push(*event))
        .expect("fixture should hash");

    assert_eq!(digest.len(), 64);
    assert_eq!(
        events.first(),
        Some(&IsoDownloadProgress {
            stage: IsoDownloadProgressStage::Verifying,
            completed_bytes: Some(0),
            total_bytes: Some(1024 * 1024 + 17),
        })
    );
    assert_eq!(
        events.last().and_then(|event| event.completed_bytes),
        Some(1024 * 1024 + 17)
    );
    assert!(events.iter().all(|event| {
        event.completed_bytes.unwrap_or_default() <= event.total_bytes.unwrap_or_default()
    }));
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
fn parses_mdt_fingerprint_values_and_generates_uuid_v4_sessions() {
    let script = "x?foo=1&w=abc%2B123&bar=2; rticks=+1787061234567";
    assert_eq!(extract_mdt_w(script).as_deref(), Some("abc%2B123"));
    assert_eq!(extract_rticks(script).as_deref(), Some("1787061234567"));
    let session = new_session_id().expect("session ID should be generated");
    assert_eq!(session.len(), 36);
    assert_eq!(session.as_bytes()[14], b'4');
    assert!(matches!(session.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
}

#[test]
fn microsoft_rate_limit_errors_do_not_echo_remote_payloads_unbounded() {
    let error = parse_languages(
        r#"{"Errors":[{"Type":9,"Value":"715-123130 https://example.invalid/?token=secret"}]}"#,
    )
    .expect_err("rate limit should fail");
    let message = error.to_string();
    assert!(message.contains("715-123130"));
    assert!(!message.contains("token=secret"));

    let sentinel = parse_languages(
        r#"{"Errors":[{"Type":0,"Value":"Sentinel marked this request as rejected."}]}"#,
    )
    .expect_err("Sentinel rejection should fail");
    assert!(sentinel.to_string().contains("retry later or use --iso"));
}

#[test]
fn aria2_adapter_keeps_the_signed_url_off_argv_and_honors_the_output_name() {
    let root = fixture();
    let fake_aria2 = root.join("aria2c");
    fs::write(
        &fake_aria2,
        "#!/bin/sh\n\
         for argument in \"$@\"; do\n\
           case \"$argument\" in\n\
             --max-redirect=*|--dir=*|--out=*) exit 91 ;;\n\
             *token=*) exit 92 ;;\n\
           esac\n\
         done\n\
         url=\n\
         output=\n\
         while IFS= read -r line; do\n\
           case \"$line\" in\n\
             '  out='*) output=${line#*out=} ;;\n\
             '  '*) ;;\n\
             *) url=$line ;;\n\
           esac\n\
         done\n\
         case \"$url\" in\n\
           https://software.download.prss.microsoft.com/*token=*) ;;\n\
           *) exit 93 ;;\n\
         esac\n\
         [ \"$output\" = '.windows.iso.lsw-download' ] || exit 94\n\
         printf downloaded > \"$output\"\n",
    )
    .expect("fake aria2 should be written");
    fs::set_permissions(&fake_aria2, fs::Permissions::from_mode(0o700))
        .expect("fake aria2 should be executable");

    let resolved = ResolvedWindowsIso {
        product_id: "product".to_owned(),
        sku_id: "sku".to_owned(),
        language: "English".to_owned(),
        architecture: "x64".to_owned(),
        filename: "windows.iso".to_owned(),
        expected_sha256: "0".repeat(64),
        expires_at: None,
        download_url: SecretDownloadUrl::parse(
            "https://software.download.prss.microsoft.com/windows.iso?token=secret",
        )
        .expect("fixture URL should be accepted"),
    };
    let temporary = root.join(".windows.iso.lsw-download");
    download_with_aria2(&fake_aria2, &resolved, &temporary, &mut |_| {})
        .expect("fake aria2 should receive a valid input-file request");
    assert_eq!(
        fs::read(&temporary).expect("download should exist"),
        b"downloaded"
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[test]
#[ignore = "requires live Microsoft endpoints"]
fn resolves_live_microsoft_iso_without_exposing_its_token() {
    let resolved = MicrosoftIsoResolver::new()
        .resolve(&MicrosoftIsoRequest::default())
        .expect("live Microsoft resolver should succeed");
    assert!(is_sha256(&resolved.expected_sha256));
    assert_eq!(resolved.architecture, "x64");
    assert!(!format!("{resolved:?}").contains("P1="));
}
