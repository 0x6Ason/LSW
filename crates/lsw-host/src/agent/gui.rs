// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use lsw_core::{
    decode_process_id, read_frame, write_frame, DesktopLiveShareRequest, Frame, FrameKind,
    GuiIconRequest, GuiInputEvent, GuiStartRequest, GuiWindowAction, GuiWindowClosed,
    GuiWindowDamage, GuiWindowDragHint, GuiWindowReady, GuiWindowResize, LiveShareStatus,
    CAPABILITY_DESKTOP_LIVE_SHARE_V1, CAPABILITY_GUI_ICON_V1, CAPABILITY_GUI_LAUNCH_V1,
    CAPABILITY_GUI_WINDOW_V3,
};

use super::{agent_error, AgentClient};

const GUI_CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct GuiWindowSession {
    pub(super) stream: TcpStream,
    pub(super) ready: GuiWindowReady,
}

pub struct GuiWindowReader {
    pub(super) stream: TcpStream,
}

pub struct GuiWindowWriter {
    pub(super) stream: TcpStream,
}

#[derive(Debug)]
pub enum GuiWindowEvent {
    Ready(GuiWindowReady),
    Damage(GuiWindowDamage),
    DragHint(GuiWindowDragHint),
    Action(GuiWindowAction),
    Closed(GuiWindowClosed),
}

impl AgentClient {
    #[allow(dead_code)]
    pub fn run_gui(mut self, request: &GuiStartRequest) -> Result<u32, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_GUI_LAUNCH_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiStart, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::Started => Ok(decode_process_id(&response.payload)?),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid GUI launch response".into()),
        }
    }

    pub fn open_gui_window(
        mut self,
        request: &GuiStartRequest,
    ) -> Result<GuiWindowSession, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_GUI_WINDOW_V3)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiWindowOpen, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::GuiWindowReady => Ok(GuiWindowSession {
                stream: self.stream,
                ready: GuiWindowReady::decode(&response.payload)?,
            }),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid seamless GUI response".into()),
        }
    }

    pub fn gui_icon(
        mut self,
        request: &GuiIconRequest,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_GUI_ICON_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiIcon, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::GuiIconData => Ok(response.payload),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid GUI icon response".into()),
        }
    }

    pub fn configure_desktop_live_share(
        mut self,
        request: &DesktopLiveShareRequest,
    ) -> Result<LiveShareStatus, Box<dyn std::error::Error>> {
        self.require_capability(CAPABILITY_DESKTOP_LIVE_SHARE_V1)?;
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::DesktopLiveShareConfigure, request.encode()?),
        )?;
        let response = read_frame(&mut self.stream)?;
        match response.kind {
            FrameKind::LiveShareStatus => Ok(LiveShareStatus::decode(&response.payload)?),
            FrameKind::Error => Err(agent_error(&response.payload).into()),
            _ => Err("agent returned an invalid desktop live-share response".into()),
        }
    }
}

impl GuiWindowSession {
    pub fn split(
        self,
    ) -> Result<(GuiWindowReady, GuiWindowReader, GuiWindowWriter), Box<dyn std::error::Error>>
    {
        let writer = self.stream.try_clone()?;
        writer.set_write_timeout(Some(GUI_CONTROL_WRITE_TIMEOUT))?;
        Ok((
            self.ready,
            GuiWindowReader {
                stream: self.stream,
            },
            GuiWindowWriter { stream: writer },
        ))
    }
}

impl GuiWindowReader {
    pub fn read_event(&mut self) -> Result<GuiWindowEvent, Box<dyn std::error::Error>> {
        let frame = read_frame(&mut self.stream)?;
        match frame.kind {
            FrameKind::GuiWindowReady => Ok(GuiWindowEvent::Ready(GuiWindowReady::decode(
                &frame.payload,
            )?)),
            FrameKind::GuiWindowDamage => Ok(GuiWindowEvent::Damage(GuiWindowDamage::decode(
                &frame.payload,
            )?)),
            FrameKind::GuiWindowDragHint => Ok(GuiWindowEvent::DragHint(
                GuiWindowDragHint::decode(&frame.payload)?,
            )),
            FrameKind::GuiWindowAction => Ok(GuiWindowEvent::Action(GuiWindowAction::decode(
                &frame.payload,
            )?)),
            FrameKind::GuiWindowClosed => Ok(GuiWindowEvent::Closed(GuiWindowClosed::decode(
                &frame.payload,
            )?)),
            FrameKind::Error => Err(agent_error(&frame.payload).into()),
            _ => Err("agent returned an invalid seamless GUI event".into()),
        }
    }
}

impl GuiWindowWriter {
    pub fn send_input(&mut self, event: GuiInputEvent) -> Result<(), Box<dyn std::error::Error>> {
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiWindowInput, event.encode()?),
        )?;
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), Box<dyn std::error::Error>> {
        write_frame(
            &mut self.stream,
            &Frame::new(
                FrameKind::GuiWindowResize,
                GuiWindowResize { width, height }.encode()?,
            ),
        )?;
        Ok(())
    }

    pub fn window_action(
        &mut self,
        action: GuiWindowAction,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !matches!(action, GuiWindowAction::Maximize | GuiWindowAction::Restore) {
            return Err("host may send only explicit maximize or restore state".into());
        }
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiWindowAction, action.encode()),
        )?;
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        write_frame(
            &mut self.stream,
            &Frame::new(FrameKind::GuiWindowClose, Vec::new()),
        )?;
        Ok(())
    }
}

impl Drop for GuiWindowWriter {
    fn drop(&mut self) {
        // The reader runs on a cloned descriptor in the presenter thread. A
        // plain descriptor drop would leave that blocking read alive after an
        // early rendering/input error, so the guest would never observe EOF or
        // reclaim its captured window and injected input state. Socket
        // shutdown applies to the shared connection and wakes both endpoints.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
