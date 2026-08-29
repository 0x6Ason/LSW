// SPDX-License-Identifier: GPL-3.0-or-later

//! Protocol, process-lifecycle, service, and configuration regression tests.
//!
//! Keeping these fixtures beside the agent but outside `main.rs` makes the
//! production control flow reviewable without weakening private-item access.

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

#[cfg(windows)]
#[test]
fn windows_pipe_job_terminates_a_spawned_descendant() {
    let mut child = spawn_program(
        "cmd.exe",
        &["/D", "/Q", "/C", "ping.exe -n 30 127.0.0.1 >NUL"],
        None,
        &ProcessEnvironment::default(),
        false,
    )
    .expect("cmd should start inside a Job Object");
    let leader = child.process.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    // Observe kernel-owned Job membership instead of treating shell output
    // as readiness. This keeps the lifecycle assertion independent of
    // PowerShell cold-start and Start-Process scheduling delays.
    let descendant = loop {
        let process_ids = child
            .tree
            .as_ref()
            .expect("spawned process should retain its Job Object")
            .process_ids()
            .expect("Job Object process membership should be queryable");
        if let Some(process_id) = process_ids
            .into_iter()
            .find(|process_id| *process_id != leader)
        {
            break process_id;
        }
        assert!(
            Instant::now() < deadline,
            "cmd did not start its ping descendant within 10 seconds"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let (_, terminated) = child
        .terminate()
        .expect("Job Object termination should succeed");
    assert!(terminated);
    assert!(
        process_tree::wait_for_process_id_exit(descendant, 2_000)
            .expect("descendant state should be queryable"),
        "the Job Object descendant was still running after session cancellation"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn windows_job_ffi_layout_matches_the_x64_sdk_abi() {
    assert_eq!(process_tree::ffi_layout_sizes(), (64, 144, 28));
}

#[cfg(windows)]
#[test]
fn windows_conpty_process_starts_inside_a_job() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["cmd.exe".to_owned()],
        working_directory: None,
    };
    let process = windows_conpty::spawn_shell(
        &request,
        TerminalSize {
            columns: 80,
            rows: 25,
        },
    )
    .expect("ConPTY shell should start suspended, join its Job, and resume");
    process
        .job
        .terminate(SESSION_CANCEL_EXIT_CODE)
        .expect("ConPTY Job termination should succeed");
    assert_eq!(
        windows_conpty::wait_for_process(&process.process)
            .expect("ConPTY process should become signalled"),
        SESSION_CANCEL_EXIT_CODE
    );
}

#[cfg(windows)]
#[test]
fn windows_conpty_retries_only_transient_console_driver_startup() {
    assert!(windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        1
    ));
    assert!(windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        19
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0003_u32 as i32,
        20
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0005_u32 as i32,
        1
    ));
    assert!(!windows_conpty::should_retry_pseudo_console_create(
        0x8007_0057_u32 as i32,
        1
    ));
}

#[cfg(windows)]
#[test]
fn windows_conpty_process_round_trips_input_and_output() {
    let request = StartRequest {
        kind: SessionKind::Shell,
        argv: vec!["cmd.exe".to_owned()],
        working_directory: None,
    };
    let mut process = windows_conpty::spawn_shell(
        &request,
        TerminalSize {
            columns: 80,
            rows: 25,
        },
    )
    .expect("ConPTY shell should start");
    process
        .input
        .write_all(b"echo LSW_CONPTY_DIRECT_OK & exit\r")
        .and_then(|()| process.input.flush())
        .expect("ConPTY input should be writable");
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit_code = loop {
        if let Some(code) = windows_conpty::wait_for_process_timeout(&process.process, 100)
            .expect("ConPTY process wait should succeed")
        {
            break code;
        }
        assert!(
            Instant::now() < deadline,
            "ConPTY command did not exit after receiving input"
        );
    };
    process
        .job
        .terminate(SESSION_CANCEL_EXIT_CODE)
        .expect("ConPTY descendants should terminate");
    drop(process.console);
    let mut output = Vec::new();
    process
        .output
        .read_to_end(&mut output)
        .expect("ConPTY output should be readable to EOF");
    assert_eq!(exit_code, 0);
    assert!(String::from_utf8_lossy(&output).contains("LSW_CONPTY_DIRECT_OK"));
}

#[cfg(not(windows))]
#[test]
fn non_windows_agent_does_not_advertise_conpty() {
    let capabilities = agent_capabilities();
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_CONPTY_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_TERMINAL_RESIZE_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_USER_ACCOUNT_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_USER_ACCOUNT_ROLE_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_POWER_HIBERNATE_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_TRIM_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_SHUTDOWN_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_WINDOWS_SUDO_V1));
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_GUI_LAUNCH_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_CONTROL_V1));
    assert!(capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_SESSION_LEASE_V1));
}

#[cfg(windows)]
#[test]
fn windows_agent_advertises_native_os_operations() {
    let capabilities = agent_capabilities();
    for expected in [
        lsw_core::CAPABILITY_CONPTY_V1,
        lsw_core::CAPABILITY_TERMINAL_RESIZE_V1,
        lsw_core::CAPABILITY_POWER_HIBERNATE_V1,
        lsw_core::CAPABILITY_USER_ACCOUNT_V1,
        lsw_core::CAPABILITY_USER_ACCOUNT_ROLE_V1,
        lsw_core::CAPABILITY_MAINTENANCE_TRIM_V1,
        lsw_core::CAPABILITY_MAINTENANCE_SHUTDOWN_V1,
        lsw_core::CAPABILITY_WINDOWS_SUDO_V1,
        lsw_core::CAPABILITY_GUI_LAUNCH_V1,
        lsw_core::CAPABILITY_GUI_ICON_V1,
        lsw_core::CAPABILITY_DESKTOP_LIVE_SHARE_V1,
    ] {
        assert!(capabilities.iter().any(|capability| capability == expected));
    }
    assert!(!capabilities
        .iter()
        .any(|capability| capability == lsw_core::CAPABILITY_MAINTENANCE_HIBERNATE_V1));
}

#[test]
fn native_shutdown_arguments_do_not_force_open_applications() {
    assert_eq!(WINDOWS_SHUTDOWN_ARGUMENTS, ["/s", "/t", "0", "/d", "p:0:0"]);
    assert!(!WINDOWS_SHUTDOWN_ARGUMENTS
        .iter()
        .any(|argument| argument.eq_ignore_ascii_case("/f")));
}

fn controlled_test_connection(
    token: String,
) -> (TcpStream, Receiver<Result<(), String>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let active_sessions = Arc::new(AtomicUsize::new(1));
    let server_sessions = Arc::clone(&active_sessions);
    let (done_sender, done_receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = {
            let _slot = SessionSlot(server_sessions);
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).map_err(|error| error.to_string())
        };
        let _ = done_sender.send(result);
    });

    let mut stream = TcpStream::connect(address).expect("client should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout should apply");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("hello response should arrive");
    assert_eq!(response.kind, FrameKind::HelloOk);
    let hello = ServerHello::decode(&response.payload).expect("server hello should decode");
    assert!(hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_SESSION_CONTROL_V1));
    assert!(hello
        .capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_SESSION_LEASE_V1));
    (stream, done_receiver, active_sessions)
}

#[test]
fn network_agent_rejects_the_helper_only_hibernate_frame() {
    let (mut stream, done, active_sessions) = controlled_test_connection("9".repeat(64));
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::MaintenanceHibernate, Vec::new()),
    )
    .expect("helper-only frame should be sent");
    let response = read_frame(&mut stream).expect("protocol rejection should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported"));
    drop(stream);
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish")
        .expect_err("helper-only frame should fail")
        .contains("unsupported"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(windows)]
#[test]
fn windows_conpty_shell_sends_exit_after_normal_completion() {
    let token = "d".repeat(64);
    let (mut stream, done, active_sessions) = controlled_test_connection(token);
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("terminal read timeout should apply");
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::SessionOptions,
            SessionOptions {
                cancel_on_disconnect: true,
            }
            .encode(),
        ),
    )
    .expect("session options should be sent");
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionLease, SessionLease::standard().encode()),
    )
    .expect("session lease should be sent");
    let request = TerminalStartRequest {
        size: TerminalSize::new(25, 80).expect("terminal size should be valid"),
        request: StartRequest {
            kind: SessionKind::Shell,
            argv: vec!["cmd.exe".to_owned()],
            working_directory: None,
        },
    };
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::TerminalStart,
            request.encode().expect("terminal request should encode"),
        ),
    )
    .expect("terminal start should be sent");
    if let Err(error) = write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::Stdin,
            b"echo LSW_CONPTY_EXIT_OK & exit\r".to_vec(),
        ),
    ) {
        panic!(
            "terminal input should be sent: {error}; server={:?}",
            done.recv_timeout(Duration::from_secs(2))
        );
    }

    let mut output = Vec::new();
    let exit_code = loop {
        let frame = read_frame(&mut stream).unwrap_or_else(|error| {
            panic!(
                "terminal completion frame should arrive: {error}; output={:?}; server={:?}",
                String::from_utf8_lossy(&output),
                done.try_recv()
            )
        });
        match frame.kind {
            FrameKind::Stdout => output.extend(frame.payload),
            FrameKind::Exit => {
                break lsw_core::decode_exit(&frame.payload).expect("exit should decode")
            }
            FrameKind::Error => panic!(
                "terminal returned an error: {}",
                String::from_utf8_lossy(&frame.payload)
            ),
            other => panic!("unexpected terminal frame {other:?}"),
        }
    };
    assert_eq!(exit_code, 0);
    assert!(String::from_utf8_lossy(&output).contains("LSW_CONPTY_EXIT_OK"));
    drop(stream);
    done.recv_timeout(Duration::from_secs(5))
        .expect("server session should finish")
        .expect("server session should succeed");
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
fn send_session_options(stream: &mut TcpStream) {
    let options = SessionOptions {
        cancel_on_disconnect: true,
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::SessionOptions, options.encode()),
    )
    .expect("session options should be sent");
}

#[cfg(unix)]
fn send_session_lease(stream: &mut TcpStream, timeout_millis: u32) {
    let lease = SessionLease::new(timeout_millis).expect("test lease should be valid");
    write_frame(stream, &Frame::new(FrameKind::SessionLease, lease.encode()))
        .expect("session lease should be sent");
}

#[cfg(unix)]
fn send_exec(stream: &mut TcpStream, argv: &[&str]) {
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        working_directory: None,
    };
    write_frame(
        stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");
}

#[cfg(unix)]
fn send_waiting_descendant_tree(stream: &mut TcpStream) {
    // outer sh -> inner sh -> sleep. Every process inherits the session
    // output pipes; killing only the outer process would make bridge joins
    // and the session slot hang until sleep exits.
    send_exec(
        stream,
        &[
            "sh",
            "-c",
            "sh -c 'sleep 30 & printf tree-ready; wait' & wait",
        ],
    );
    let ready = read_frame(stream).expect("descendant readiness should arrive");
    assert_eq!(ready.kind, FrameKind::Stdout);
    assert_eq!(ready.payload, b"tree-ready");
}

#[cfg(unix)]
fn collect_process(stream: &mut TcpStream) -> (Vec<u8>, i32) {
    let mut stdout = Vec::new();
    loop {
        let frame = read_frame(stream).expect("process response should arrive");
        match frame.kind {
            FrameKind::Stdout => stdout.extend(frame.payload),
            FrameKind::Stderr => {}
            FrameKind::Exit => return (stdout, lsw_core::decode_exit(&frame.payload).unwrap()),
            other => panic!("unexpected process frame {other:?}"),
        }
    }
}

#[cfg(unix)]
fn assert_session_released(done: Receiver<Result<(), String>>, active_sessions: Arc<AtomicUsize>) {
    done.recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect("server session should succeed");
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn authenticated_cancel_terminates_a_controlled_process() {
    let (mut stream, done, active_sessions) = controlled_test_connection("d".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("cancel should be sent");

    assert_eq!(
        collect_process(&mut stream),
        (Vec::new(), SESSION_CANCEL_EXIT_CODE)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn authenticated_environment_and_working_directory_reach_the_child() {
    let root = std::env::temp_dir().join(format!("lsw-agent-env-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("working directory should be created");
    let (mut stream, done, active_sessions) = controlled_test_connection("e".repeat(64));
    send_session_options(&mut stream);
    let environment = ProcessEnvironment::new(vec![(
        "LSW_TEST_ENV".to_owned(),
        "environment-ok".to_owned(),
    )])
    .expect("environment should validate");
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::ProcessEnvironment,
            environment.encode().expect("environment should encode"),
        ),
    )
    .expect("environment should be sent");
    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf '%s|%s' \"$LSW_TEST_ENV\" \"$PWD\"".to_owned(),
        ],
        working_directory: Some(root.to_string_lossy().into_owned()),
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");
    let (stdout, code) = collect_process(&mut stream);
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8(stdout).expect("output should be UTF-8"),
        format!("environment-ok|{}", root.display())
    );
    drop(stream);
    assert_session_released(done, active_sessions);
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[cfg(unix)]
#[test]
fn authenticated_signal_terminates_the_process_tree_with_exact_status() {
    let (mut stream, done, active_sessions) = controlled_test_connection("f".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionSignal, SessionSignal::Interrupt.encode()),
    )
    .expect("signal should be sent");
    assert_eq!(collect_process(&mut stream).1, 130);
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn detached_run_survives_the_client_disconnect_until_completion() {
    let root = std::env::temp_dir().join(format!("lsw-agent-detach-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("fixture directory should be created");
    let marker = root.join("complete.marker");
    let (mut stream, done, active_sessions) = controlled_test_connection("a".repeat(64));
    write_frame(
        &mut stream,
        &Frame::new(
            FrameKind::SessionOptions,
            SessionOptions {
                cancel_on_disconnect: false,
            }
            .encode(),
        ),
    )
    .expect("session options should be sent");
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionDetach, Vec::new()),
    )
    .expect("detach request should be sent");
    let request = StartRequest {
        kind: SessionKind::Run,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!("sleep 1; printf complete > '{}'", marker.display()),
        ],
        working_directory: None,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");
    let started = read_frame(&mut stream).expect("start acknowledgement should arrive");
    assert_eq!(started.kind, FrameKind::Started);
    assert!(lsw_core::decode_process_id(&started.payload).unwrap() > 0);
    drop(stream);
    done.recv_timeout(Duration::from_millis(500))
        .expect("detached start should release its connection slot promptly")
        .expect("server session should succeed");
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        fs::read_to_string(&marker).expect("detached process should write marker"),
        "complete"
    );
    fs::remove_dir_all(root).expect("fixture should be removed");
}

#[cfg(unix)]
#[test]
fn session_control_is_unavailable_before_authentication() {
    let expected_token = "7".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture should connect");
        handle_connection(stream, &expected_token).map_err(|error| error.to_string())
    });
    let mut stream = TcpStream::connect(address).expect("client should connect");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token: "8".repeat(64),
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("authentication error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("authentication"));
    assert!(server
        .join()
        .expect("fixture should not panic")
        .expect_err("authentication should fail")
        .contains("authentication"));
}

#[cfg(unix)]
#[test]
fn controlled_disconnect_terminates_process_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("e".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    drop(stream);

    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn leased_session_expires_and_releases_its_process_tree() {
    let (mut stream, done, active_sessions) = controlled_test_connection("4".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 1_000);
    send_waiting_descendant_tree(&mut stream);

    assert!(
        read_frame(&mut stream).is_err(),
        "lease expiry closes the half-open transport instead of risking a blocking error write"
    );
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("leased server session should finish promptly")
        .expect_err("lease expiry should fail the session")
        .contains("lease expired"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn lease_expiry_unblocks_output_backpressure_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("8".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 1_000);
    send_exec(&mut stream, &["sh", "-c", "exec yes lsw-lease-output"]);

    // Deliberately never read process output. The agent output bridge will
    // eventually block in TCP write while holding its shared writer lock.
    // Lease expiry must use socket shutdown, not that lock, to free it.
    assert!(done
        .recv_timeout(Duration::from_secs(3))
        .expect("backpressured leased session should finish promptly")
        .expect_err("lease expiry should fail the session")
        .contains("lease expired"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    drop(stream);
}

#[cfg(unix)]
#[test]
fn timely_heartbeats_keep_an_idle_leased_session_alive() {
    let (mut stream, done, active_sessions) = controlled_test_connection("5".repeat(64));
    send_session_options(&mut stream);
    send_session_lease(&mut stream, 2_000);
    send_waiting_descendant_tree(&mut stream);

    // The total duration exceeds one lease, while every individual gap is
    // comfortably below it. This proves idle-but-healthy sessions survive.
    for _ in 0..9 {
        thread::sleep(Duration::from_millis(250));
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionHeartbeat, Vec::new()),
        )
        .expect("heartbeat should be sent");
    }
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("cancel should be sent");
    assert_eq!(
        collect_process(&mut stream),
        (Vec::new(), SESSION_CANCEL_EXIT_CODE)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn heartbeat_requires_a_leased_session_and_empty_payload() {
    for (token, with_lease, payload) in [("a", false, Vec::new()), ("b", true, vec![1])] {
        let (mut stream, done, active_sessions) = controlled_test_connection(token.repeat(64));
        send_session_options(&mut stream);
        if with_lease {
            send_session_lease(&mut stream, 5_000);
        }
        send_waiting_descendant_tree(&mut stream);
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::SessionHeartbeat, payload),
        )
        .expect("invalid heartbeat should be sent");

        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("requires a leased"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }
}

#[cfg(unix)]
#[test]
fn lease_requires_options_and_can_appear_only_once_before_start() {
    let (mut legacy, legacy_done, legacy_sessions) = controlled_test_connection("c".repeat(64));
    send_session_lease(&mut legacy, 5_000);
    let response = read_frame(&mut legacy).expect("legacy lease should be rejected");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
    assert!(legacy_done
        .recv_timeout(Duration::from_secs(2))
        .expect("legacy request should finish promptly")
        .is_err());
    assert_eq!(legacy_sessions.load(Ordering::Acquire), 0);

    let (mut duplicate, duplicate_done, duplicate_sessions) =
        controlled_test_connection("d".repeat(64));
    send_session_options(&mut duplicate);
    send_session_lease(&mut duplicate, 5_000);
    send_session_lease(&mut duplicate, 5_000);
    let response = read_frame(&mut duplicate).expect("duplicate lease should be rejected");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("one SESSION_LEASE"));
    assert!(duplicate_done
        .recv_timeout(Duration::from_secs(2))
        .expect("duplicate request should finish promptly")
        .is_err());
    assert_eq!(duplicate_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn controlled_frames_reject_nonempty_cancel_payloads() {
    let (mut stream, done, active_sessions) = controlled_test_connection("9".repeat(64));
    send_session_options(&mut stream);
    send_waiting_descendant_tree(&mut stream);
    write_frame(&mut stream, &Frame::new(FrameKind::SessionCancel, [1]))
        .expect("malformed cancel should be sent");

    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("empty payload"));
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .is_err());
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn normal_leader_exit_cleans_background_descendants_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("6".repeat(64));
    send_session_options(&mut stream);
    // The shell exits normally without waiting for this background sleep.
    // The sleep inherits stdout/stderr, so an agent that owns only the
    // leader blocks in its output bridge instead of sending EXIT.
    send_exec(&mut stream, &["sh", "-c", "sleep 30 & printf normal-tree"]);

    assert_eq!(collect_process(&mut stream), (b"normal-tree".to_vec(), 0));
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn session_options_only_prefix_process_start_requests() {
    for invalid_kind in [
        FrameKind::SessionOptions,
        FrameKind::Ping,
        FrameKind::FileGet,
        FrameKind::FilePut,
        FrameKind::SessionCancel,
        FrameKind::StdinClose,
    ] {
        let token_byte = match invalid_kind {
            FrameKind::SessionOptions => 'a',
            FrameKind::Ping => 'b',
            FrameKind::FileGet => 'c',
            FrameKind::FilePut => 'd',
            FrameKind::SessionCancel => 'e',
            FrameKind::StdinClose => 'f',
            _ => unreachable!(),
        };
        let (mut stream, done, active_sessions) =
            controlled_test_connection(token_byte.to_string().repeat(64));
        send_session_options(&mut stream);
        let payload = if invalid_kind == FrameKind::SessionOptions {
            SessionOptions {
                cancel_on_disconnect: true,
            }
            .encode()
        } else {
            Vec::new()
        };
        write_frame(&mut stream, &Frame::new(invalid_kind, payload))
            .expect("invalid controlled request should be sent");
        let response = read_frame(&mut stream).expect("protocol error should arrive");
        assert_eq!(response.kind, FrameKind::Error);
        assert!(String::from_utf8_lossy(&response.payload).contains("must be followed"));
        assert!(done
            .recv_timeout(Duration::from_secs(2))
            .expect("server session should finish promptly")
            .is_err());
        assert_eq!(active_sessions.load(Ordering::Acquire), 0);
    }
}

#[cfg(unix)]
#[test]
fn unknown_session_option_flags_are_rejected_before_spawn() {
    let (mut stream, done, active_sessions) = controlled_test_connection("2".repeat(64));
    write_frame(&mut stream, &Frame::new(FrameKind::SessionOptions, [2]))
        .expect("unknown options should be sent");
    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unknown flags"));

    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect_err("unknown option flags should fail")
        .contains("unknown flags"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn legacy_control_frame_is_rejected_without_starting_a_process() {
    let (mut stream, done, active_sessions) = controlled_test_connection("3".repeat(64));
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::SessionCancel, Vec::new()),
    )
    .expect("legacy cancel should be sent");
    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("unsupported request"));
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .is_err());
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn controlled_stdin_close_delivers_eof_without_cancelling() {
    let (mut stream, done, active_sessions) = controlled_test_connection("f".repeat(64));
    send_session_options(&mut stream);
    send_exec(
        &mut stream,
        &["sh", "-c", "IFS= read -r value; printf controlled-eof"],
    );
    write_frame(&mut stream, &Frame::new(FrameKind::StdinClose, Vec::new()))
        .expect("stdin close should be sent");

    assert_eq!(
        collect_process(&mut stream),
        (b"controlled-eof".to_vec(), 0)
    );
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn controlled_child_stdin_failure_terminates_process_and_releases_slot() {
    let (mut stream, done, active_sessions) = controlled_test_connection("0".repeat(64));
    // macOS can defer delivery of the peer-side EPIPE while reaping the
    // exec'd shell. Keep the assertion bounded, but use the same five-second
    // allowance as the loopback E2E fixtures instead of the generic
    // two-second protocol timeout.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stdin-failure timeout should apply");
    send_session_options(&mut stream);
    send_exec(
        &mut stream,
        &["sh", "-c", "exec 0<&-; printf ready; exec sleep 5"],
    );

    let ready = read_frame(&mut stream).expect("child readiness should arrive");
    assert_eq!(ready.kind, FrameKind::Stdout);
    assert_eq!(ready.payload, b"ready");
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Stdin, b"input after child closed stdin"),
    )
    .expect("stdin payload should be sent");

    let response = read_frame(&mut stream).expect("protocol error should arrive");
    assert_eq!(response.kind, FrameKind::Error);
    assert!(String::from_utf8_lossy(&response.payload).contains("child stdin"));
    drop(stream);
    assert!(done
        .recv_timeout(Duration::from_secs(2))
        .expect("server session should finish promptly")
        .expect_err("child stdin failure should fail the session")
        .contains("child stdin"));
    assert_eq!(active_sessions.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn legacy_half_close_remains_stdin_eof_not_cancellation() {
    let (mut stream, done, active_sessions) = controlled_test_connection("1".repeat(64));
    send_exec(
        &mut stream,
        &["sh", "-c", "IFS= read -r value; printf legacy-eof"],
    );
    stream
        .shutdown(Shutdown::Write)
        .expect("legacy write side should close");

    assert_eq!(collect_process(&mut stream), (b"legacy-eof".to_vec(), 0));
    drop(stream);
    assert_session_released(done, active_sessions);
}

#[cfg(unix)]
#[test]
fn authenticated_loopback_exec_streams_output_and_exit_status() {
    let token = "a".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("fixture should connect");
        handle_connection(stream, &expected_token).expect("agent request should succeed");
    });

    let mut stream = TcpStream::connect(address).expect("client should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
    )
    .expect("hello should be sent");
    let response = read_frame(&mut stream).expect("hello response should arrive");
    assert_eq!(response.kind, FrameKind::HelloOk);
    ServerHello::decode(&response.payload).expect("server hello should decode");

    let request = StartRequest {
        kind: SessionKind::Exec,
        argv: vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf stdout-bytes; printf stderr-bytes >&2; exit 7".to_owned(),
        ],
        working_directory: None,
    };
    write_frame(
        &mut stream,
        &Frame::new(FrameKind::Start, request.encode().unwrap()),
    )
    .expect("start should be sent");

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = loop {
        let frame = read_frame(&mut stream).expect("process frame should arrive");
        match frame.kind {
            FrameKind::Stdout => stdout.extend(frame.payload),
            FrameKind::Stderr => stderr.extend(frame.payload),
            FrameKind::Exit => break lsw_core::decode_exit(&frame.payload).unwrap(),
            other => panic!("unexpected process frame {other:?}"),
        }
    };
    assert_eq!(stdout, b"stdout-bytes");
    assert_eq!(stderr, b"stderr-bytes");
    assert_eq!(exit, 7);
    drop(stream);
    server.join().expect("agent fixture should finish");
}

#[test]
fn nonblocking_accepted_socket_waits_for_the_bounded_handshake() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let token = "a".repeat(64);
    let server_token = token.clone();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("server should accept");
        stream
            .set_nonblocking(true)
            .expect("fixture stream should become nonblocking");
        handle_connection(stream, &server_token).expect("delayed handshake should succeed");
    });

    let mut client = std::net::TcpStream::connect(address).expect("client should connect");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let hello = ClientHello {
        version: AGENT_PROTOCOL_VERSION,
        token,
    };
    write_frame(
        &mut client,
        &Frame::new(
            FrameKind::Hello,
            hello.encode().expect("hello should encode"),
        ),
    )
    .expect("client should write HELLO");
    assert_eq!(
        read_frame(&mut client).expect("server should answer").kind,
        FrameKind::HelloOk
    );
    write_frame(&mut client, &Frame::new(FrameKind::Ping, Vec::new()))
        .expect("client should write PING");
    assert_eq!(
        read_frame(&mut client).expect("server should answer").kind,
        FrameKind::Pong
    );
    server.join().expect("server should not panic");
}

#[cfg(unix)]
#[test]
fn authenticated_loopback_file_transfer_preserves_unicode_and_bytes() {
    fn connect(token: &str) -> (TcpStream, thread::JoinHandle<Result<(), String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let expected_token = token.to_owned();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fixture should connect");
            handle_connection(stream, &expected_token).map_err(|error| error.to_string())
        });
        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        let response = read_frame(&mut stream).expect("hello response should arrive");
        assert_eq!(response.kind, FrameKind::HelloOk);
        ServerHello::decode(&response.payload).expect("server hello should decode");
        (stream, server)
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be valid")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("lsw-agent-e2e-{nonce}"));
    fs::create_dir(&root).expect("fixture directory should be created");
    let destination = root.join("résumé-данные.bin");
    let destination_text = destination.to_string_lossy().into_owned();
    let contents = b"binary\0payload\xff\nUTF-8:\xf0\x9f\x9a\x80";
    let token = "b".repeat(64);

    let (mut upload, upload_server) = connect(&token);
    let put = FilePutRequest {
        destination: destination_text.clone(),
        length: contents.len() as u64,
    };
    write_frame(
        &mut upload,
        &Frame::new(FrameKind::FilePut, put.encode().unwrap()),
    )
    .expect("upload request should be sent");
    assert_eq!(
        read_frame(&mut upload)
            .expect("upload ready should arrive")
            .kind,
        FrameKind::Pong
    );
    for chunk in contents.chunks(7) {
        write_frame(
            &mut upload,
            &Frame::new(FrameKind::FileData, chunk.to_vec()),
        )
        .expect("upload data should be sent");
    }
    write_frame(
        &mut upload,
        &Frame::new(
            FrameKind::FileDone,
            encode_file_length(contents.len() as u64),
        ),
    )
    .expect("upload completion should be sent");
    let completion = read_frame(&mut upload).expect("upload completion should arrive");
    assert_eq!(completion.kind, FrameKind::FileDone);
    assert_eq!(
        decode_file_length(&completion.payload).unwrap(),
        contents.len() as u64
    );
    drop(upload);
    upload_server
        .join()
        .expect("upload fixture should finish")
        .expect("upload should succeed");
    assert_eq!(fs::read(&destination).unwrap(), contents);

    let (mut download, download_server) = connect(&token);
    let get = FileGetRequest {
        source: destination_text,
    };
    write_frame(
        &mut download,
        &Frame::new(FrameKind::FileGet, get.encode().unwrap()),
    )
    .expect("download request should be sent");
    let mut received = Vec::new();
    loop {
        let frame = read_frame(&mut download).expect("download frame should arrive");
        match frame.kind {
            FrameKind::FileData => received.extend(frame.payload),
            FrameKind::FileDone => {
                assert_eq!(
                    decode_file_length(&frame.payload).unwrap(),
                    received.len() as u64
                );
                break;
            }
            other => panic!("unexpected download frame {other:?}"),
        }
    }
    drop(download);
    download_server
        .join()
        .expect("download fixture should finish")
        .expect("download should succeed");
    assert_eq!(received, contents);

    fs::remove_dir_all(root).expect("fixture directory should be removable");
}

#[cfg(unix)]
#[test]
fn independent_authenticated_sessions_run_concurrently() {
    fn connect(address: SocketAddr, token: &str) -> TcpStream {
        let mut stream = TcpStream::connect(address).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(
            &mut stream,
            &Frame::new(FrameKind::Hello, hello.encode().unwrap()),
        )
        .expect("hello should be sent");
        assert_eq!(
            read_frame(&mut stream)
                .expect("hello response should arrive")
                .kind,
            FrameKind::HelloOk
        );
        stream
    }

    fn start(stream: &mut TcpStream, script: &str) {
        let request = StartRequest {
            kind: SessionKind::Exec,
            argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            working_directory: None,
        };
        write_frame(
            stream,
            &Frame::new(FrameKind::Start, request.encode().unwrap()),
        )
        .expect("start should be sent");
    }

    fn collect(stream: &mut TcpStream) -> (Vec<u8>, i32) {
        let mut stdout = Vec::new();
        loop {
            let frame = read_frame(stream).expect("process frame should arrive");
            match frame.kind {
                FrameKind::Stdout => stdout.extend(frame.payload),
                FrameKind::Stderr => {}
                FrameKind::Exit => return (stdout, lsw_core::decode_exit(&frame.payload).unwrap()),
                other => panic!("unexpected process frame {other:?}"),
            }
        }
    }

    let token = "c".repeat(64);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener should bind");
    let address = listener
        .local_addr()
        .expect("listener should have an address");
    let expected_token = token.clone();
    let server = thread::spawn(move || {
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().expect("fixture should connect");
            let token = expected_token.clone();
            sessions.push(thread::spawn(move || {
                handle_connection(stream, &token).map_err(|error| error.to_string())
            }));
        }
        for session in sessions {
            session
                .join()
                .expect("session should not panic")
                .expect("session should succeed");
        }
    });

    let mut blocked = connect(address, &token);
    start(
        &mut blocked,
        "IFS= read -r value; printf 'first-%s' \"$value\"",
    );

    let mut independent = connect(address, &token);
    start(&mut independent, "printf second");
    assert_eq!(collect(&mut independent), (b"second".to_vec(), 0));

    write_frame(
        &mut blocked,
        &Frame::new(FrameKind::Stdin, b"ready\n".to_vec()),
    )
    .expect("blocked session input should be sent");
    assert_eq!(collect(&mut blocked), (b"first-ready".to_vec(), 0));
    drop(independent);
    drop(blocked);
    server.join().expect("server fixture should finish");
}
