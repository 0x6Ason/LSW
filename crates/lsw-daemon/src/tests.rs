// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::BufReader;

use lsw_core::{read_daemon_reply, write_daemon_request};

use super::*;

fn supervisor() -> Supervisor {
    Supervisor::new(
        StateStore::new(std::env::temp_dir().join("lswd-dispatch-test")),
        HostCapabilities::unavailable(lsw_core::HostPlatform::Linux),
        GuestCpuTuning::default(),
    )
}

#[test]
fn ping_reports_the_protocol_version_and_features() {
    let DaemonResponse::Pong(info) =
        dispatch(DaemonRequest::Ping, &mut supervisor()).expect("ping should work")
    else {
        panic!("ping must answer with pong");
    };
    assert!(info.is_compatible());
    assert_eq!(info.protocol, DAEMON_PROTOCOL_VERSION);
    assert_eq!(
        info.features,
        ["suspend", "resume", "hibernate", "balloon", "idle-exit"]
    );
    assert_eq!(info.pid, std::process::id());
    assert_eq!(
        info.lines()[..3],
        [
            "PONG".to_owned(),
            format!("PROTOCOL={DAEMON_PROTOCOL_VERSION}"),
            "FEATURES=suspend,resume,hibernate,balloon,idle-exit".to_owned(),
        ]
    );
}

#[test]
fn unknown_instances_are_rejected_without_side_effects() {
    let mut supervisor = supervisor();
    for request in [
        DaemonRequest::Status {
            name: "missing-instance".to_owned(),
        },
        DaemonRequest::Stop {
            name: "missing-instance".to_owned(),
            force: true,
        },
        DaemonRequest::Suspend {
            name: "missing-instance".to_owned(),
        },
        DaemonRequest::Balloon {
            name: "missing-instance".to_owned(),
            memory_mib: 512,
        },
    ] {
        assert!(dispatch(request, &mut supervisor).is_err());
    }
}

#[test]
fn malformed_and_oversized_requests_get_an_error_reply() {
    for raw in [
        "START everything now\n".to_owned(),
        "{\"request\":\"stop\",\"name\":\"x\"}\n".to_owned(),
        format!(
            "{{\"request\":\"show\",\"name\":\"{}\"}}\n",
            "x".repeat(5000)
        ),
    ] {
        let (client, server) = UnixStream::pair().expect("socket pair should be created");
        let mut supervisor = supervisor();
        let server_thread = thread::spawn(move || {
            handle_connection(server, &mut supervisor).map_err(|error| error.to_string())
        });
        let mut client_writer = client.try_clone().expect("client should clone");
        std::io::Write::write_all(&mut client_writer, raw.as_bytes()).expect("request is sent");
        let reply = read_daemon_reply(&mut BufReader::new(client))
            .expect("reply decodes")
            .expect("daemon answered");
        assert!(
            matches!(reply, DaemonReply::Error(_)),
            "{raw:?} -> {reply:?}"
        );
        server_thread
            .join()
            .expect("handler thread finishes")
            .expect("handler reports success after answering");
    }
}

#[test]
fn typed_requests_round_trip_through_a_connection() {
    let (client, server) = UnixStream::pair().expect("socket pair should be created");
    let mut supervisor = supervisor();
    let server_thread = thread::spawn(move || {
        handle_connection(server, &mut supervisor).map_err(|error| error.to_string())
    });
    let mut client_writer = client.try_clone().expect("client should clone");
    write_daemon_request(&mut client_writer, &DaemonRequest::Ping).expect("request is sent");
    let reply = read_daemon_reply(&mut BufReader::new(client))
        .expect("reply decodes")
        .expect("daemon answered");
    assert!(matches!(reply, DaemonReply::Ok(DaemonResponse::Pong(_))));
    server_thread
        .join()
        .expect("handler thread finishes")
        .expect("handler reports success");
}
