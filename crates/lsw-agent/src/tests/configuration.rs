// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[cfg(unix)]
#[test]
fn shell_fallback_reaches_a_known_program() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["definitely-not-an-lsw-shell".to_owned(), "sh".to_owned()],
        working_directory: None,
    };
    let mut child = spawn_request(&request, &ProcessEnvironment::default(), false)
        .expect("sh fallback should start");
    child.kill().expect("fixture process should stop");
    assert!(
        child.tree.is_none(),
        "successful teardown must disarm owner"
    );
    child
        .kill()
        .expect("a repeated cleanup request must be a no-op");
    child.wait().expect("fixture process should be reaped");
    assert!(
        child.tree.is_none(),
        "wait and Drop must not re-signal PGID"
    );
    // Exercise Drop with an already-reaped leader and disarmed owner. It
    // must not call Child::kill on a potentially reused Unix PID.
    drop(child);
}

#[test]
fn token_parser_rejects_short_or_uppercase_secrets() {
    let root = std::env::temp_dir().join(format!("lsw-agent-token-{}", std::process::id()));
    fs::write(&root, "abcd").expect("fixture should be written");
    assert!(read_token(&root).is_err());
    fs::write(&root, "A".repeat(64)).expect("fixture should be updated");
    assert!(read_token(&root).is_err());
    fs::remove_file(root).expect("fixture should be removed");
}

#[cfg(windows)]
fn create_test_directory_reparse(target: &Path, link: &Path) {
    if let Err(error) = std::os::windows::fs::symlink_dir(target, link) {
        if error.kind() != io::ErrorKind::PermissionDenied && error.raw_os_error() != Some(1314) {
            panic!("fixture directory link should be created: {error}");
        }
        let status = Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("junction helper should start");
        assert!(
            status.success(),
            "an unprivileged junction fixture should be available"
        );
    }
}

#[cfg(windows)]
#[test]
fn abnormal_gui_host_eof_wakes_a_silent_desktop_companion() {
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture should bind");
        let address = listener.local_addr().expect("fixture should have address");
        let connector = thread::spawn(move || {
            TcpStream::connect(address).expect("fixture client should connect")
        });
        let server = listener.accept().expect("fixture should accept").0;
        let client = connector.join().expect("connector should not panic");
        (server, client)
    }

    let (mut host_reader, host_peer) = loopback_pair();
    let (desktop_writer, mut desktop_peer) = loopback_pair();
    desktop_peer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("fixture timeout should apply");

    // Match the deadlock shape: the companion has emitted one static Ready
    // frame and will produce nothing further until its control socket closes.
    let ready = GuiWindowReady {
        process_id: 7,
        window_id: 9,
        width: 800,
        height: 600,
        title: "fixture".to_owned(),
    };
    write_frame(
        &mut desktop_peer,
        &Frame::new(FrameKind::GuiWindowReady, ready.encode().unwrap()),
    )
    .expect("desktop Ready should be sent");

    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = done_sender.send(relay_gui_controls(&mut host_reader, desktop_writer));
    });
    host_peer
        .shutdown(Shutdown::Both)
        .expect("host fixture should disconnect");
    let relay = done_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("input relay should not remain blocked after host EOF");
    assert!(
        relay.is_err(),
        "abnormal EOF must not look like explicit close"
    );
    assert!(
        read_frame(&mut desktop_peer).is_err(),
        "the companion side must observe EOF so it can release input and retain the HWND"
    );
}

#[cfg(windows)]
#[test]
fn gui_close_relay_keeps_forwarding_save_dialog_input() {
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("fixture should bind");
        let address = listener.local_addr().expect("fixture should have address");
        let connector = thread::spawn(move || {
            TcpStream::connect(address).expect("fixture client should connect")
        });
        let server = listener.accept().expect("fixture should accept").0;
        let client = connector.join().expect("connector should not panic");
        (server, client)
    }

    let (mut host_reader, mut host_peer) = loopback_pair();
    let (desktop_writer, mut desktop_peer) = loopback_pair();
    desktop_peer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("fixture timeout should apply");
    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = done_sender.send(relay_gui_controls(&mut host_reader, desktop_writer));
    });

    write_frame(
        &mut host_peer,
        &Frame::new(FrameKind::GuiWindowClose, Vec::new()),
    )
    .expect("close request should be sent");
    let close = read_frame(&mut desktop_peer).expect("close request should be forwarded");
    assert_eq!(close.kind, FrameKind::GuiWindowClose);
    assert!(close.payload.is_empty());
    assert!(matches!(
        done_receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    let input = GuiInputEvent::Focus { focused: true };
    write_frame(
        &mut host_peer,
        &Frame::new(FrameKind::GuiWindowInput, input.encode().unwrap()),
    )
    .expect("save-dialog input should be sent after close");
    let forwarded =
        read_frame(&mut desktop_peer).expect("save-dialog input should remain forwardable");
    assert_eq!(forwarded.kind, FrameKind::GuiWindowInput);
    assert_eq!(GuiInputEvent::decode(&forwarded.payload).unwrap(), input);

    host_peer
        .shutdown(Shutdown::Both)
        .expect("host fixture should disconnect");
    assert!(done_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("input relay should stop after host EOF")
        .is_err());
}

#[cfg(windows)]
#[test]
fn gui_event_relay_accepts_only_bounded_drag_hints() {
    let hint = GuiWindowDragHint {
        x: 12,
        y: 34,
        action: Some(GuiWindowAction::Move),
    };
    assert!(validate_gui_event_frame(&Frame::new(
        FrameKind::GuiWindowDragHint,
        hint.encode().unwrap(),
    ))
    .is_ok());
    assert!(validate_gui_event_frame(&Frame::new(
        FrameKind::GuiWindowDragHint,
        vec![4, 0, 0, 0, 0, 0, 0, 0, 0],
    ))
    .is_err());
}

#[cfg(windows)]
#[test]
fn windows_identity_file_replacement_is_atomic_and_repeatable() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-identity-{nonce}"));
    fs::create_dir(&root).expect("fixture directory should be created");
    let token = root.join("agent.token");
    fs::write(&token, b"old\n").expect("fixture token should be written");
    windows_path::replace_file(&token, b"new\n").expect("token should be replaced");
    windows_path::replace_file(&token, b"newer\n").expect("token should replace again");
    assert_eq!(
        fs::read(&token).expect("token should be readable"),
        b"newer\n"
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[cfg(windows)]
#[test]
fn windows_upload_publish_is_atomic_create_only_and_cleans_failures() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-upload-{nonce}"));
    fs::create_dir(&root).expect("fixture directory should be created");

    let first_temporary = root.join("first.upload");
    let destination = root.join("published.bin");
    let mut first = windows_path::UploadFile::create(&first_temporary)
        .expect("temporary upload should be created");
    first
        .write_all(b"complete payload")
        .expect("upload should be written");
    first.sync_all().expect("upload should be durable");
    first
        .publish_new(&destination)
        .expect("unused destination should publish");
    drop(first);
    assert_eq!(
        fs::read(&destination).expect("published file should be readable"),
        b"complete payload"
    );
    assert!(!first_temporary.exists(), "rename should consume temporary");

    let second_temporary = root.join("second.upload");
    let mut second = windows_path::UploadFile::create(&second_temporary)
        .expect("second temporary upload should be created");
    second
        .write_all(b"must not replace")
        .expect("second upload should be written");
    second.sync_all().expect("second upload should be durable");
    let collision = second
        .publish_new(&destination)
        .expect_err("an existing destination must win");
    assert_eq!(
        collision.kind(),
        io::ErrorKind::AlreadyExists,
        "NT object-name collision should retain create-only error semantics"
    );
    drop(second);
    assert_eq!(
        fs::read(&destination).expect("original destination should remain readable"),
        b"complete payload",
        "failed publish must never overwrite destination"
    );
    assert!(
        !second_temporary.exists(),
        "failed upload should be deleted through its owned handle"
    );

    fs::remove_dir_all(root).expect("fixture directory should be removed");
}

#[cfg(windows)]
#[test]
fn concurrent_windows_upload_publish_has_exactly_one_winner() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-upload-race-{nonce}"));
    fs::create_dir(&root).expect("fixture directory should be created");
    let destination = Arc::new(root.join("winner.bin"));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut contenders = Vec::new();
    for (ordinal, contents) in [b"first".as_slice(), b"second".as_slice()]
        .into_iter()
        .enumerate()
    {
        let temporary = root.join(format!("contender-{ordinal}.upload"));
        let destination = Arc::clone(&destination);
        let barrier = Arc::clone(&barrier);
        contenders.push(thread::spawn(move || {
            let mut upload = windows_path::UploadFile::create(&temporary)
                .expect("contender should create its temporary file");
            upload
                .write_all(contents)
                .expect("contender should write its payload");
            upload
                .sync_all()
                .expect("contender should sync its payload");
            barrier.wait();
            let published = upload.publish_new(&destination).is_ok();
            drop(upload);
            (published, temporary)
        }));
    }
    let results = contenders
        .into_iter()
        .map(|contender| contender.join().expect("contender should not panic"))
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|(published, _)| *published).count(),
        1,
        "create-only rename must have exactly one winner"
    );
    let contents = fs::read(destination.as_ref()).expect("winner should be readable");
    assert!(
        matches!(contents.as_slice(), b"first" | b"second"),
        "destination must contain one complete contender payload"
    );
    assert!(
        results.iter().all(|(_, temporary)| !temporary.exists()),
        "both the moved winner and discarded loser temporaries must disappear"
    );

    fs::remove_dir_all(root).expect("fixture directory should be removed");
}

#[cfg(windows)]
#[test]
fn windows_upload_publish_rejects_a_reparse_destination_parent() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-upload-link-{nonce}"));
    let target = root.join("target");
    let link = root.join("link");
    fs::create_dir_all(&target).expect("fixture target should be created");
    create_test_directory_reparse(&target, &link);

    let temporary = root.join("reparse.upload");
    let escaped = target.join("escaped.bin");
    let mut upload =
        windows_path::UploadFile::create(&temporary).expect("temporary upload should be created");
    upload
        .write_all(b"must not escape")
        .expect("upload should be written");
    assert!(
        upload.publish_new(&link.join("escaped.bin")).is_err(),
        "publish must reject a reparse component"
    );
    drop(upload);
    assert!(
        !escaped.exists(),
        "publish must not follow the directory link"
    );
    assert!(!temporary.exists(), "rejected upload should be discarded");

    fs::remove_dir(&link).expect("fixture directory link should be removed");
    fs::remove_dir_all(root).expect("fixture directory should be removed");
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn windows_file_rename_information_matches_the_x64_kernel_abi() {
    assert_eq!(
        windows_path::file_rename_information_abi(),
        (8, 16, 20, 24, 16),
        "FILE_RENAME_INFORMATION and IO_STATUS_BLOCK must match the x64 SDK ABI"
    );
}

#[cfg(windows)]
#[test]
fn windows_upload_publish_is_anchored_against_parent_path_replacement() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-upload-anchor-{nonce}"));
    let parent = root.join("parent");
    let anchored_parent = root.join("anchored-parent");
    let escape = root.join("escape");
    fs::create_dir_all(&parent).expect("fixture parent should be created");
    fs::create_dir(&escape).expect("escape directory should be created");

    // Keep the source beside (rather than inside) the destination parent so
    // Windows permits the test to rename that directory while it is open.
    // Production uploads create the source in the same directory, which is an
    // additional obstacle to this attack, but the publish primitive must be
    // safe even without relying on that incidental sharing restriction.
    let temporary = root.join("payload.upload");
    let destination = parent.join("published.bin");
    let mut upload =
        windows_path::UploadFile::create(&temporary).expect("temporary upload should be created");
    upload
        .write_all(b"handle anchored payload")
        .expect("upload should be written");
    upload.sync_all().expect("upload should be durable");
    upload
        .publish_new_after_parent_open(&destination, || {
            fs::rename(&parent, &anchored_parent)
                .expect("validated parent should be renamed while its handle is open");
            create_test_directory_reparse(&escape, &parent);
        })
        .expect("publish should remain anchored to the opened parent");
    drop(upload);

    assert_eq!(
        fs::read(anchored_parent.join("published.bin"))
            .expect("payload should land in the handle-anchored directory"),
        b"handle anchored payload"
    );
    assert!(
        !escape.join("published.bin").exists(),
        "replacement junction target must never receive the upload"
    );
    assert!(
        !temporary.exists(),
        "handle-relative rename should consume the temporary name"
    );

    fs::remove_dir(&parent).expect("replacement directory link should be removed");
    fs::remove_dir_all(root).expect("fixture directory should be removed");
}

#[cfg(windows)]
#[test]
fn windows_late_identity_volume_updates_the_live_authenticator() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let fixture = std::env::temp_dir().join(format!("lsw-agent-late-identity-{nonce}"));
    let identity = fixture.join("identity");
    let identity_root = identity.join("lsw");
    let data = fixture.join("data");
    fs::create_dir_all(&identity_root).expect("identity fixture should be created");
    fs::create_dir_all(&data).expect("data fixture should be created");

    let drive = (b'D'..=b'Z')
        .rev()
        .map(char::from)
        .find(|letter| !Path::new(&format!("{letter}:\\")).exists())
        .expect("an unused test drive letter should be available");
    let drive_name = format!("{drive}:");
    let old_token = "a".repeat(64);
    let new_token = "b".repeat(64);
    let token_file = data.join("agent.token");
    fs::write(&token_file, format!("{old_token}\n")).expect("old token should be written");
    fs::write(
        identity_root.join(CLONE_IDENTITY_MARKER_FILE),
        b"LSW-CLONE-IDENTITY\n",
    )
    .expect("identity marker should be written");
    fs::write(
        identity_root.join(CLONE_IDENTITY_NAME_FILE),
        b"late-identity\n",
    )
    .expect("identity name should be written");
    fs::write(
        identity_root.join(CLONE_IDENTITY_TOKEN_FILE),
        format!("{new_token}\n"),
    )
    .expect("new token should be written");

    let live_token = Arc::new(Mutex::new(old_token));
    let test_identity_root = identity_root.clone();
    watch_for_clone_identity_with_timing(
        token_file.clone(),
        Arc::downgrade(&live_token),
        Duration::from_secs(3),
        Duration::from_millis(100),
        Duration::from_millis(25),
        Duration::from_millis(100),
        move |token_file| {
            apply_clone_identity_from_roots(token_file, std::iter::once(test_identity_root.clone()))
        },
    )
    .expect("identity watcher should start");
    thread::sleep(Duration::from_millis(350));
    let mounted = Command::new("subst.exe")
        .arg(&drive_name)
        .arg(&identity)
        .status()
        .expect("subst should start")
        .success();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut applied = false;
    while Instant::now() < deadline {
        applied = live_token
            .lock()
            .map(|token| token.as_str() == new_token)
            .unwrap_or(false);
        if applied {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let token_contents = fs::read_to_string(&token_file);
    let name_contents = fs::read_to_string(data.join("instance.name"));
    let unmounted = Command::new("subst.exe")
        .args([&drive_name, "/D"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    fs::remove_dir_all(fixture).expect("fixture should be removed");

    assert!(mounted, "identity fixture drive should mount");
    assert!(applied, "late identity should update the in-memory token");
    assert_eq!(
        token_contents
            .expect("live token file should be readable")
            .trim(),
        new_token
    );
    assert_eq!(
        name_contents
            .expect("live identity name should be readable")
            .trim(),
        "late-identity"
    );
    assert!(unmounted, "identity fixture drive should unmount");
}

#[cfg(windows)]
#[test]
fn windows_volume_guid_enumeration_reaches_the_system_volume() {
    let roots = windows_path::volume_roots().expect("Windows volumes should enumerate");
    assert!(
        roots.iter().any(|root| root.join("Windows").is_dir()),
        "a volume GUID path should reach the running Windows installation"
    );
}

#[test]
fn session_limit_is_bounded() {
    let arguments = vec![
        "--token-file".into(),
        "token.txt".into(),
        "--max-sessions".into(),
        "4".into(),
    ];
    let configuration = Configuration::parse(&arguments).expect("configuration should parse");
    assert_eq!(configuration.max_sessions, 4);

    let invalid = vec![
        "--token-file".into(),
        "token.txt".into(),
        "--max-sessions".into(),
        "0".into(),
    ];
    assert!(Configuration::parse(&invalid).is_err());
}

#[test]
fn service_mode_composes_with_existing_options() {
    let arguments = vec![
        "--service".into(),
        "--token-file".into(),
        "C:\\ProgramData\\LSW\\agent.token".into(),
        "--listen".into(),
        "127.0.0.1:55040".into(),
        "--max-sessions".into(),
        "8".into(),
    ];
    let configuration = Configuration::parse(&arguments).expect("configuration should parse");

    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::Agent);
    assert_eq!(configuration.listen, "127.0.0.1:55040".parse().unwrap());
    assert_eq!(configuration.max_sessions, 8);
    assert_eq!(
        configuration.token_file,
        PathBuf::from("C:\\ProgramData\\LSW\\agent.token")
    );
}

#[test]
fn license_helper_is_loopback_only_and_uses_a_distinct_service_mode() {
    let configuration = Configuration::parse(&[
        "--license-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "127.0.0.1:5041".into(),
    ])
    .expect("license helper configuration should parse");
    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::LicenseHelper);
    assert!(configuration.listen.ip().is_loopback());

    assert!(Configuration::parse(&[
        "--license-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "0.0.0.0:5041".into(),
    ])
    .is_err());
}

#[test]
fn user_helper_is_loopback_only_and_uses_a_distinct_service_mode() {
    let configuration = Configuration::parse(&[
        "--user-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "127.0.0.1:5042".into(),
    ])
    .expect("user helper configuration should parse");
    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::UserHelper);
    assert!(configuration.listen.ip().is_loopback());

    assert!(Configuration::parse(&[
        "--user-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "0.0.0.0:5042".into(),
    ])
    .is_err());
}

#[test]
fn maintenance_helper_is_loopback_only_and_uses_a_distinct_service_mode() {
    let configuration = Configuration::parse(&[
        "--maintenance-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "127.0.0.1:5043".into(),
    ])
    .expect("maintenance helper configuration should parse");
    assert!(configuration.service);
    assert_eq!(configuration.service_kind, ServiceKind::MaintenanceHelper);
    assert!(configuration.listen.ip().is_loopback());

    assert!(Configuration::parse(&[
        "--maintenance-helper".into(),
        "--token-file".into(),
        "agent.token".into(),
        "--listen".into(),
        "0.0.0.0:5043".into(),
    ])
    .is_err());
}

#[test]
fn product_key_shape_is_strict_without_recording_a_real_key() {
    assert!(valid_product_key(b"AAAAA-BBBBB-CCCCC-DDDDD-EEEEE"));
    assert!(!valid_product_key(b"AAAAA-BBBBB-CCCCC-DDDDD"));
    assert!(!valid_product_key(b"AAAAA_BBBBB_CCCCC_DDDDD_EEEEE"));
    assert!(!valid_product_key(b"aaaaa-bbbbb-ccccc-ddddd-eeeee"));
}

#[cfg(not(windows))]
#[test]
fn service_mode_fails_before_accessing_files_off_windows() {
    let error = run(vec![
        "--service".into(),
        "--token-file".into(),
        "a-file-that-must-not-be-read".into(),
    ])
    .expect_err("service mode must be Windows-only");

    assert_eq!(error.to_string(), "--service is only supported on Windows");
}

#[cfg(windows)]
#[test]
fn service_mode_requires_scm_before_accessing_files() {
    let error = run(vec![
        "--service".into(),
        "--token-file".into(),
        "a-file-that-must-not-be-read".into(),
    ])
    .expect_err("a normal test process must not connect to SCM as a service");

    assert!(
        error
            .to_string()
            .contains("--service must be started by SCM"),
        "unexpected SCM rejection: {error}"
    );
}

#[test]
fn shutdown_channel_wakes_or_disconnects_the_accept_wait() {
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(()).expect("shutdown should be queued");
    assert!(wait_for_shutdown(&receiver, Duration::from_secs(1)));

    let (sender, receiver) = mpsc::sync_channel(1);
    drop(sender);
    assert!(shutdown_requested(&receiver));
}

#[test]
fn normal_exit_wins_a_cancel_race() {
    assert_eq!(cancel_session_end(false), SessionEnd::Normal);
    assert_eq!(cancel_session_end(true), SessionEnd::Cancelled);
}

#[test]
fn normal_exit_wins_a_lease_expiry_race() {
    assert_eq!(lease_session_end(false), SessionEnd::Normal);
    assert_eq!(lease_session_end(true), SessionEnd::LeaseExpired);
}

#[test]
fn windows_command_line_quotes_empty_and_space_containing_arguments() {
    assert_eq!(windows_command_line("", &[]), "\"\"");
    assert_eq!(
        windows_command_line("C:\\Program Files\\pwsh.exe", &["hello world"]),
        "\"C:\\Program Files\\pwsh.exe\" \"hello world\""
    );
}

#[test]
fn windows_command_line_quotes_quotes_and_trailing_backslashes() {
    assert_eq!(
        windows_command_line("tool.exe", &["a\"b"]),
        "tool.exe \"a\\\"b\""
    );
    assert_eq!(
        windows_command_line("tool.exe", &["C:\\path with space\\"]),
        "tool.exe \"C:\\path with space\\\\\""
    );
    assert_eq!(
        windows_command_line("tool.exe", &["plain\\"]),
        "tool.exe plain\\"
    );
}
