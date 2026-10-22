// SPDX-License-Identifier: GPL-3.0-or-later

//! The typed request/reply protocol between `lsw` clients and `lswd`.
//!
//! One request is one newline-terminated JSON object on the private Unix
//! socket; the daemon answers with one newline-terminated JSON object that is
//! either `{"ok": <response>}` or `{"error": "<message>"}`. Both sides share
//! these types, so neither re-parses `KEY=VALUE` text. The CLI renders a
//! response with [`DaemonResponse::lines`], which keeps the `STATE=`, `QMP=`,
//! `ACTIVE=`, and `PONG` lines that scripts have always matched.

use std::fmt;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{InstanceState, LaunchPhase, LswError, Result, WindowsProfile};

/// Incremented whenever a request or reply changes shape. A client only talks
/// to a daemon whose `PING` reports the same version.
pub const DAEMON_PROTOCOL_VERSION: u16 = 3;
/// Upper bound on one encoded request line, including its newline.
pub const MAX_DAEMON_REQUEST_BYTES: usize = 4096;
/// Upper bound on one encoded reply line, including its newline.
pub const MAX_DAEMON_REPLY_BYTES: usize = 2 * 1024 * 1024;

/// Everything a client can ask the daemon to do.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum DaemonRequest {
    Ping,
    List,
    Show { name: String },
    Plan { name: String, phase: LaunchPhase },
    Start { name: String, phase: LaunchPhase },
    Status { name: String },
    Suspend { name: String },
    Resume { name: String },
    Hibernate { name: String },
    Activity { name: String },
    Balloon { name: String, memory_mib: u32 },
    Stop { name: String, force: bool },
}

/// A successful answer. Data-bearing replies are typed; lifecycle operations
/// that only have something to say to a person return `Notice`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DaemonResponse {
    Pong(DaemonInfo),
    Instances { instances: Vec<InstanceSummary> },
    Manifest { encoded: String },
    Plan(LaunchPlanSummary),
    Status(InstanceStatus),
    Started(StartedInstance),
    Notice { lines: Vec<String> },
}

/// What `PING` reports about the running daemon.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonInfo {
    pub protocol: u16,
    pub features: Vec<String>,
    pub pid: u32,
    pub rss_kib: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstanceSummary {
    pub name: String,
    pub state: InstanceState,
    pub profile: WindowsProfile,
}

/// The QEMU command line the daemon would launch, rendered for display.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LaunchPlanSummary {
    pub helper_commands: Vec<String>,
    pub command: String,
    pub notes: Vec<String>,
    pub missing_capabilities: Vec<String>,
}

/// Whether a QEMU process for the instance is known to exist.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Liveness {
    /// QMP answered, or the daemon owns the process.
    Active,
    /// No process evidence remains.
    Inactive,
    /// Process evidence exists but QMP could not be consulted.
    Unknown,
}

impl fmt::Display for Liveness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Active => "true",
            Self::Inactive => "false",
            Self::Unknown => "unknown",
        })
    }
}

/// The reconciled state of one instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstanceStatus {
    pub state: InstanceState,
    /// QEMU's own `query-status` word (`running`, `paused`, ...), or `None`
    /// when QMP was unavailable.
    pub qmp: Option<String>,
    pub active: Liveness,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StartedInstance {
    pub name: String,
    pub phase: LaunchPhase,
    pub qmp_socket: PathBuf,
    pub log: PathBuf,
}

impl DaemonInfo {
    /// Whether this daemon speaks the protocol compiled into the client.
    pub const fn is_compatible(&self) -> bool {
        self.protocol == DAEMON_PROTOCOL_VERSION
    }

    /// The `PONG` block `lsw daemon status` prints.
    pub fn lines(&self) -> Vec<String> {
        vec![
            "PONG".to_owned(),
            format!("PROTOCOL={}", self.protocol),
            format!("FEATURES={}", self.features.join(",")),
            format!("PID={}", self.pid),
            format!("RSS_KIB={}", self.rss_kib.unwrap_or(0)),
        ]
    }
}

impl DaemonResponse {
    /// The human-readable and script-stable text form of a response.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Pong(info) => info.lines(),
            Self::Instances { instances } => instances
                .iter()
                .map(|instance| {
                    format!(
                        "INSTANCE\t{}\t{}\t{}",
                        instance.name, instance.state, instance.profile
                    )
                })
                .collect(),
            Self::Manifest { encoded } => encoded.lines().map(str::to_owned).collect(),
            Self::Plan(plan) => {
                let mut lines: Vec<String> = plan
                    .helper_commands
                    .iter()
                    .map(|helper| format!("HELPER\t{helper}"))
                    .collect();
                lines.push(format!("COMMAND\t{}", plan.command));
                lines.extend(plan.notes.iter().map(|note| format!("NOTE\t{note}")));
                lines.extend(
                    plan.missing_capabilities
                        .iter()
                        .map(|missing| format!("MISSING\t{missing}")),
                );
                lines
            }
            Self::Status(status) => vec![
                format!("STATE={}", status.state),
                format!("QMP={}", status.qmp.as_deref().unwrap_or("unavailable")),
                format!("ACTIVE={}", status.active),
            ],
            Self::Started(started) => vec![
                format!(
                    "instance {} started in {} phase",
                    started.name, started.phase
                ),
                format!("QMP={}", started.qmp_socket.display()),
                format!("LOG={}", started.log.display()),
            ],
            Self::Notice { lines } => lines.clone(),
        }
    }
}

/// The wire envelope around a response or a rejection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonReply {
    Ok(DaemonResponse),
    Error(String),
}

/// Writes one request line.
pub fn write_daemon_request(writer: &mut impl Write, request: &DaemonRequest) -> Result<()> {
    write_bounded_line(writer, request, MAX_DAEMON_REQUEST_BYTES, "daemon request")
}

/// Writes one reply line.
pub fn write_daemon_reply(writer: &mut impl Write, reply: &DaemonReply) -> Result<()> {
    write_bounded_line(writer, reply, MAX_DAEMON_REPLY_BYTES, "daemon reply")
}

/// Reads one request line; `None` means the peer closed without sending one.
pub fn read_daemon_request(reader: &mut impl BufRead) -> Result<Option<DaemonRequest>> {
    read_bounded_line(reader, MAX_DAEMON_REQUEST_BYTES, "daemon request")
}

/// Reads one reply line; `None` means the daemon closed without answering.
pub fn read_daemon_reply(reader: &mut impl BufRead) -> Result<Option<DaemonReply>> {
    read_bounded_line(reader, MAX_DAEMON_REPLY_BYTES, "daemon reply")
}

fn write_bounded_line<T: Serialize>(
    writer: &mut impl Write,
    value: &T,
    limit: usize,
    what: &str,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(value)
        .map_err(|error| LswError::Protocol(format!("could not encode {what}: {error}")))?;
    encoded.push(b'\n');
    if encoded.len() > limit {
        return Err(LswError::Protocol(format!("{what} exceeds {limit} bytes")));
    }
    writer.write_all(&encoded)?;
    writer.flush()?;
    Ok(())
}

fn read_bounded_line<T: for<'de> Deserialize<'de>>(
    reader: &mut impl BufRead,
    limit: usize,
    what: &str,
) -> Result<Option<T>> {
    let mut line = Vec::new();
    let mut bounded = reader.by_ref().take(limit as u64 + 1);
    let bytes = bounded.read_until(b'\n', &mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    if line.len() > limit || line.last() != Some(&b'\n') {
        return Err(LswError::Protocol(format!(
            "{what} is larger than {limit} bytes or not newline-terminated"
        )));
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|error| LswError::Protocol(format!("invalid {what}: {error}")))
}

impl From<serde_json::Error> for LswError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn round_trip_request(request: &DaemonRequest) -> DaemonRequest {
        let mut buffer = Vec::new();
        write_daemon_request(&mut buffer, request).expect("request encodes");
        assert!(buffer.ends_with(b"\n"));
        read_daemon_request(&mut io::Cursor::new(buffer))
            .expect("request decodes")
            .expect("request is present")
    }

    #[test]
    fn requests_round_trip_with_stable_wire_names() {
        for request in [
            DaemonRequest::Ping,
            DaemonRequest::List,
            DaemonRequest::Show {
                name: "win-dev".to_owned(),
            },
            DaemonRequest::Plan {
                name: "win-dev".to_owned(),
                phase: LaunchPhase::Install,
            },
            DaemonRequest::Start {
                name: "win-dev".to_owned(),
                phase: LaunchPhase::Run,
            },
            DaemonRequest::Balloon {
                name: "win-dev".to_owned(),
                memory_mib: 2048,
            },
            DaemonRequest::Stop {
                name: "win-dev".to_owned(),
                force: true,
            },
        ] {
            assert_eq!(round_trip_request(&request), request);
        }
        let encoded = serde_json::to_string(&DaemonRequest::Start {
            name: "win-dev".to_owned(),
            phase: LaunchPhase::Run,
        })
        .expect("encodes");
        assert_eq!(
            encoded,
            "{\"request\":\"start\",\"name\":\"win-dev\",\"phase\":\"run\"}"
        );
    }

    #[test]
    fn replies_round_trip_and_render_the_script_facing_lines() {
        let status = DaemonReply::Ok(DaemonResponse::Status(InstanceStatus {
            state: InstanceState::Running,
            qmp: Some("running".to_owned()),
            active: Liveness::Active,
        }));
        let mut buffer = Vec::new();
        write_daemon_reply(&mut buffer, &status).expect("reply encodes");
        let decoded = read_daemon_reply(&mut io::Cursor::new(buffer))
            .expect("reply decodes")
            .expect("reply is present");
        assert_eq!(decoded, status);
        let DaemonReply::Ok(response) = decoded else {
            panic!("reply must be ok");
        };
        assert_eq!(
            response.lines(),
            ["STATE=running", "QMP=running", "ACTIVE=true"]
        );

        let unavailable = DaemonResponse::Status(InstanceStatus {
            state: InstanceState::Stopped,
            qmp: None,
            active: Liveness::Inactive,
        });
        assert_eq!(
            unavailable.lines(),
            ["STATE=stopped", "QMP=unavailable", "ACTIVE=false"]
        );

        let pong = DaemonResponse::Pong(DaemonInfo {
            protocol: DAEMON_PROTOCOL_VERSION,
            features: vec!["suspend".to_owned(), "resume".to_owned()],
            pid: 42,
            rss_kib: None,
        });
        assert_eq!(
            pong.lines(),
            [
                "PONG".to_owned(),
                format!("PROTOCOL={DAEMON_PROTOCOL_VERSION}"),
                "FEATURES=suspend,resume".to_owned(),
                "PID=42".to_owned(),
                "RSS_KIB=0".to_owned(),
            ]
        );
        assert_eq!(
            serde_json::to_string(&DaemonReply::Error("boom".to_owned())).expect("encodes"),
            "{\"error\":\"boom\"}"
        );
    }

    #[test]
    fn oversized_or_unterminated_lines_are_rejected() {
        let huge = DaemonRequest::Show {
            name: "x".repeat(MAX_DAEMON_REQUEST_BYTES),
        };
        let mut buffer = Vec::new();
        assert!(matches!(
            write_daemon_request(&mut buffer, &huge),
            Err(LswError::Protocol(_))
        ));
        assert!(matches!(
            read_daemon_request(&mut io::Cursor::new(b"{\"request\":\"ping\"".to_vec())),
            Err(LswError::Protocol(_))
        ));
        assert!(matches!(
            read_daemon_request(&mut io::Cursor::new(b"not json\n".to_vec())),
            Err(LswError::Protocol(_))
        ));
        assert!(read_daemon_request(&mut io::Cursor::new(Vec::new()))
            .expect("empty input is a closed peer")
            .is_none());
    }

    #[test]
    fn unknown_requests_are_rejected_rather_than_defaulted() {
        assert!(read_daemon_request(&mut io::Cursor::new(
            b"{\"request\":\"format-disk\",\"name\":\"x\"}\n".to_vec()
        ))
        .is_err());
        assert!(read_daemon_request(&mut io::Cursor::new(
            b"{\"request\":\"stop\",\"name\":\"x\"}\n".to_vec()
        ))
        .is_err());
    }
}
