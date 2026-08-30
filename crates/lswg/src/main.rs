// SPDX-License-Identifier: GPL-3.0-or-later

#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("lswg currently requires a Unix Wayland host");

use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use lsw_core::{
    read_frame, FolderShareTransport, FrameKind, GuiStartRequest, StateStore,
    CAPABILITY_GUI_WINDOW_V3,
};
use lsw_host::AgentClient;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lswg: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if arguments == ["--version"] {
        println!(
            "lswg {} {CAPABILITY_GUI_WINDOW_V3}",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let [flag, instance] = arguments.as_slice() else {
        return Err("usage: lswg --instance NAME".into());
    };
    if flag != "--instance" {
        return Err("usage: lswg --instance NAME".into());
    }

    let request = read_launch_request()?;
    let store = StateStore::new(state_root()?);
    let manifest = store.load(instance)?;
    if manifest.default_user.as_deref() != Some(request.user_name.as_str()) {
        return Err("GUI request user does not match the registered instance desktop user".into());
    }
    let expected_live_share = manifest
        .folder_shares
        .iter()
        .any(|share| share.transport == FolderShareTransport::LiveSmb);
    if request.mount_live_share != expected_live_share {
        return Err("GUI request live-share state does not match the instance manifest".into());
    }
    let token = store.read_agent_token(instance)?;
    let session = AgentClient::connect(&manifest, &token)?.open_gui_window(&request)?;
    let exit_code = lswg::present(session)?;
    println!("LSWG_EXIT={exit_code}");
    Ok(())
}

fn read_launch_request() -> Result<GuiStartRequest, Box<dyn std::error::Error>> {
    let mut stdin = io::stdin().lock();
    let frame = read_frame(&mut stdin)?;
    if frame.kind != FrameKind::GuiWindowOpen {
        return Err("lswg input did not contain a GUI-window request".into());
    }
    let request = GuiStartRequest::decode(&frame.payload)?;
    let mut trailing = [0_u8; 1];
    if stdin.read(&mut trailing)? != 0 {
        return Err("lswg input contained trailing data".into());
    }
    Ok(request)
}

fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(configured) = env::var_os("LSW_STATE_DIR") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set; configure LSW_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/share/lsw"))
}
