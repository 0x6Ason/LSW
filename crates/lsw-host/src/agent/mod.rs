// SPDX-License-Identifier: GPL-3.0-or-later

mod gui;
mod process;
mod transfer;

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::Duration;

use lsw_core::{
    read_frame, write_frame, ClientHello, Frame, FrameKind, InstanceManifest,
    LiveShareConfigureRequest, LiveShareStatus, ServerHello, UserCreateRequest, UserSetRoleRequest,
    WindowsSudoConfigureRequest, WindowsSudoStatus, AGENT_PROTOCOL_VERSION, CAPABILITY_CONPTY_V1,
    CAPABILITY_LIVE_SHARE_V1, CAPABILITY_MAINTENANCE_SHUTDOWN_V1, CAPABILITY_MAINTENANCE_TRIM_V1,
    CAPABILITY_USER_ACCOUNT_ROLE_V1, CAPABILITY_USER_ACCOUNT_V1, CAPABILITY_WINDOWS_SUDO_V1,
};

pub use gui::{GuiWindowEvent, GuiWindowReader, GuiWindowSession, GuiWindowWriter};
pub use process::CapturedProcess;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AgentClient {
    stream: TcpStream,
    capabilities: Vec<String>,
}

impl AgentClient {
    pub fn connect(
        manifest: &InstanceManifest,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.control_port);
        let mut stream = TcpStream::connect_timeout(&address.into(), CONNECT_TIMEOUT)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let hello = ClientHello {
            version: AGENT_PROTOCOL_VERSION,
            token: token.to_owned(),
        };
        write_frame(&mut stream, &Frame::new(FrameKind::Hello, hello.encode()?))?;
        let response = read_frame(&mut stream)?;
        match response.kind {
            FrameKind::HelloOk => {
                let hello = ServerHello::decode(&response.payload)?;
                if hello.version != AGENT_PROTOCOL_VERSION {
                    return Err(
                        format!("agent negotiated unsupported protocol {}", hello.version).into(),
                    );
                }
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                Ok(Self {
                    stream,
                    capabilities: hello.capabilities,
                })
            }
            FrameKind::Error => Err(format!(
                "agent rejected connection: {}",
                String::from_utf8_lossy(&response.payload)
            )
            .into()),
            other => Err(format!("agent returned unexpected {other:?} frame").into()),
        }
    }

    pub fn probe(mut self) -> Result<(), Box<dyn std::error::Error>> {
        write_frame(&mut self.stream, &Frame::new(FrameKind::Ping, Vec::new()))?;
        let response = read_frame(&mut self.stream)?;
        if response.kind != FrameKind::Pong || !response.payload.is_empty() {
            return Err("agent returned an invalid PONG response".into());
        }
        Ok(())
    }

    pub fn supports_conpty(&self) -> bool {
        self.has_capability(CAPABILITY_CONPTY_V1)
    }

    pub fn create_user(
        mut self,
        request: &UserCreateRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_USER_ACCOUNT_V1)?;
        let mut frame = Frame::new(FrameKind::UserCreate, request.encode()?);
        let write_result = write_frame(&mut self.stream, &frame);
        frame.payload.fill(0);
        write_result?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid user-creation response".into()),
        }
    }

    pub fn set_user_role(
        mut self,
        request: &UserSetRoleRequest,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_USER_ACCOUNT_ROLE_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::UserSetRole, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid user-role response".into()),
        }
    }

    pub fn trim(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_MAINTENANCE_TRIM_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::MaintenanceTrim, Vec::new()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid maintenance response".into()),
        }
    }

    pub fn shutdown(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_MAINTENANCE_SHUTDOWN_V1)?;
        // The guest acknowledges before shutdown.exe runs. Keep this control
        // exchange bounded in case Windows tears down the service mid-frame.
        self.stream
            .set_read_timeout(Some(SHUTDOWN_RESPONSE_TIMEOUT))?;
        self.stream
            .set_write_timeout(Some(SHUTDOWN_RESPONSE_TIMEOUT))?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::MaintenanceShutdown, Vec::new()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid shutdown response".into()),
        }
    }

    pub fn windows_sudo_status(mut self) -> Result<WindowsSudoStatus, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_WINDOWS_SUDO_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::WindowsSudoQuery, Vec::new()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::WindowsSudoStatus => Ok(WindowsSudoStatus::decode(&response.payload)?),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid Windows sudo status response".into()),
        }
    }

    pub fn configure_windows_sudo(
        mut self,
        enable: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_WINDOWS_SUDO_V1)?;
        let request = WindowsSudoConfigureRequest { enable };
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::WindowsSudoConfigure, request.encode()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid Windows sudo configuration response".into()),
        }
    }

    pub fn live_share_status(mut self) -> Result<LiveShareStatus, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_LIVE_SHARE_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::LiveShareQuery, Vec::new()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::LiveShareStatus => Ok(LiveShareStatus::decode(&response.payload)?),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid live-share status response".into()),
        }
    }

    pub fn configure_live_share(mut self, enable: bool) -> Result<(), Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_LIVE_SHARE_V1)?;
        let request = LiveShareConfigureRequest { enable };
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::LiveShareConfigure, request.encode()),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Pong if response.payload.is_empty() => Ok(()),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid live-share configuration response".into()),
        }
    }
    fn require_capability(&self, capability: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.has_capability(capability) {
            Ok(())
        } else {
            Err(format!("guest agent does not support {capability}").into())
        }
    }

    fn has_capability(&self, capability: &str) -> bool {
        self.capabilities
            .iter()
            .any(|available| available == capability)
    }
}

pub fn agent_address(manifest: &InstanceManifest) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.control_port)
}

pub(super) fn agent_error(payload: &[u8]) -> String {
    format!("guest agent: {}", String::from_utf8_lossy(payload))
}

#[cfg(test)]
mod tests;
