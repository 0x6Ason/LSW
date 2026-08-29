// SPDX-License-Identifier: GPL-3.0-or-later

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lsw_core::{HostCapabilities, StateStore};

const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);
const VIEWER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(12);

pub fn launch(
    store: &StateStore,
    name: &str,
    explicit: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if env::var_os("DISPLAY").is_none() && env::var_os("WAYLAND_DISPLAY").is_none() {
        return unavailable(
            explicit,
            &format!(
                "no graphical desktop was detected; run `lsw view {name}` from a graphical session"
            ),
        );
    }

    let capabilities = HostCapabilities::detect();
    let viewer = env::var_os("LSW_INSTALL_VIEWER")
        .map(PathBuf::from)
        .or(capabilities.remote_viewer);
    let Some(viewer) = viewer else {
        return unavailable(
            explicit,
            "remote-viewer was not found; install the optional virt-viewer package",
        );
    };
    let Some(setsid) = capabilities.setsid else {
        return unavailable(
            explicit,
            "setsid was not found; install util-linux before opening the viewer",
        );
    };

    let instance_dir = store.instance_dir(name)?;
    let socket = instance_dir.join("run/recovery-vnc.sock");
    wait_for_private_socket(&socket, name)?;

    let token = launch_token()?;
    let connection = instance_dir
        .join("run")
        .join(format!("installation-viewer-{token}.vv"));
    let status_path = instance_dir
        .join("run")
        .join(format!("viewer-launch-{token}.status"));
    let executable = env::current_exe()?;
    let mut helper = Command::new(setsid)
        .arg(executable)
        .arg("__viewer-bridge")
        .arg(&socket)
        .arg(&connection)
        .arg(&status_path)
        .arg(&viewer)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let result = wait_for_helper(&mut helper, &status_path);
    let _ = remove_regular_file(&status_path);
    if result.is_err() {
        let _ = helper.kill();
        let _ = helper.wait();
        let _ = remove_regular_file(&connection);
    }
    result?;
    println!("Opened the private LSW viewer for {name:?}.");
    Ok(())
}

pub fn bridge_command(arguments: &[OsString]) -> Result<u8, Box<dyn std::error::Error>> {
    let status_path = arguments.get(2).map(PathBuf::from);
    match run_bridge(arguments) {
        Ok(()) => Ok(0),
        Err(error) => {
            if let Some(status_path) = status_path {
                let message = error.to_string().replace(['\r', '\n'], " ");
                let _ = write_status(&status_path, &format!("ERROR\n{message}\n"));
            }
            Err(error)
        }
    }
}

fn unavailable(explicit: bool, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    if explicit {
        Err(message.to_owned().into())
    } else {
        println!("Viewer not opened: {message}.");
        Ok(())
    }
}

fn wait_for_private_socket(socket: &Path, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + SOCKET_READY_TIMEOUT;
    loop {
        match fs::symlink_metadata(socket) {
            Ok(metadata) if metadata.file_type().is_socket() => return Ok(()),
            Ok(_) => {
                return Err(
                    format!("the private display path for {name:?} is not a Unix socket").into(),
                )
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(format!("the installation display for {name:?} is not ready").into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn launch_token() -> Result<String, Box<dyn std::error::Error>> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{}-{nanos}", std::process::id()))
}

fn wait_for_helper(
    helper: &mut Child,
    status_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + HELPER_READY_TIMEOUT;
    loop {
        match read_status(status_path)? {
            Some(BridgeStatus::Ready) => return Ok(()),
            Some(BridgeStatus::Error(message)) => {
                return Err(format!("remote-viewer could not open the display: {message}").into())
            }
            None => {}
        }
        if let Some(status) = helper.try_wait()? {
            if let Some(bridge_status) = read_status(status_path)? {
                return match bridge_status {
                    BridgeStatus::Ready => Ok(()),
                    BridgeStatus::Error(message) => {
                        Err(format!("remote-viewer could not open the display: {message}").into())
                    }
                };
            }
            return Err(format!("the viewer bridge exited during startup with {status}").into());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for remote-viewer to connect".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

enum BridgeStatus {
    Ready,
    Error(String),
}

fn read_status(path: &Path) -> Result<Option<BridgeStatus>, Box<dyn std::error::Error>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if contents == "READY\n" {
        return Ok(Some(BridgeStatus::Ready));
    }
    if let Some(message) = contents.strip_prefix("ERROR\n") {
        let message = message.trim_end_matches(['\r', '\n']);
        if !message.is_empty() {
            return Ok(Some(BridgeStatus::Error(message.to_owned())));
        }
    }
    Err("the viewer bridge returned an invalid startup status".into())
}

fn run_bridge(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.len() != 5 {
        return Err("invalid internal viewer bridge arguments".into());
    }
    let socket = PathBuf::from(&arguments[0]);
    let connection = PathBuf::from(&arguments[1]);
    let status_path = PathBuf::from(&arguments[2]);
    let viewer = PathBuf::from(&arguments[3]);
    let title = arguments[4]
        .to_str()
        .filter(|value| !value.contains(['\r', '\n']))
        .ok_or("the viewer title cannot be represented safely")?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    write_connection_file(&connection, port, title)?;
    let _connection_guard = RemoveRegularFile(connection);

    let mut viewer = Command::new(viewer)
        .arg(&_connection_guard.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + VIEWER_CONNECT_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                break stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                stop_child(&mut viewer);
                return Err(error.into());
            }
        }
        match viewer.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("remote-viewer exited before connecting with {status}").into())
            }
            Ok(None) => {}
            Err(error) => {
                stop_child(&mut viewer);
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            stop_child(&mut viewer);
            return Err("remote-viewer did not connect within 10 seconds".into());
        }
        thread::sleep(Duration::from_millis(25));
    };
    drop(listener);

    let private = match UnixStream::connect(socket) {
        Ok(private) => private,
        Err(error) => {
            stop_child(&mut viewer);
            return Err(format!("could not connect the private VNC socket: {error}").into());
        }
    };
    if let Err(error) = write_status(&status_path, "READY\n") {
        stop_child(&mut viewer);
        return Err(error);
    }

    // Startup has succeeded. Later EOF or reset means that either the viewer or
    // VM display closed; it is no longer a launch failure for the waiting CLI.
    let _ = relay(stream, private);
    let _ = viewer.wait();
    Ok(())
}

fn write_connection_file(
    path: &Path,
    port: u16,
    title: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if port == 0 || title.contains(['\r', '\n']) {
        return Err("invalid private viewer connection details".into());
    }
    let contents =
        format!("[virt-viewer]\ntype=vnc\nhost=127.0.0.1\nport={port}\ntitle=LSW - {title}\n");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn write_status(path: &Path, contents: &str) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = path.with_extension("status.tmp");
    let _ = remove_regular_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn remove_regular_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to remove non-regular file {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

struct RemoveRegularFile(PathBuf);

impl Drop for RemoveRegularFile {
    fn drop(&mut self) {
        let _ = remove_regular_file(&self.0);
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn relay(mut public: TcpStream, mut private: UnixStream) -> io::Result<()> {
    let mut public_reader = public.try_clone()?;
    let mut private_writer = private.try_clone()?;
    let request = thread::spawn(move || -> io::Result<u64> {
        let copied = io::copy(&mut public_reader, &mut private_writer)?;
        private_writer.shutdown(Shutdown::Write)?;
        Ok(copied)
    });

    let response = io::copy(&mut private, &mut public);
    let _ = public.shutdown(Shutdown::Write);
    let request = request
        .join()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "viewer relay thread panicked"))?;
    request?;
    response?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn temporary_directory(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos();
        let path =
            env::temp_dir().join(format!("lsw-viewer-{label}-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).expect("temporary directory should be created");
        path
    }

    #[test]
    fn connection_file_uses_ephemeral_loopback_tcp_for_vnc() {
        let root = temporary_directory("connection");
        let path = root.join("viewer.vv");
        write_connection_file(&path, 43123, "win-dev").expect("connection should be written");
        let contents = fs::read_to_string(&path).expect("connection should be readable");
        assert!(contents.contains("type=vnc\n"));
        assert!(contents.contains("host=127.0.0.1\n"));
        assert!(contents.contains("port=43123\n"));
        assert!(!contents.contains("unix-path="));
        assert_eq!(
            fs::metadata(&path)
                .expect("metadata should be available")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn relay_is_bidirectional() {
        let root = temporary_directory("relay");
        let unix_path = root.join("display.sock");
        let unix_listener = UnixListener::bind(&unix_path).expect("Unix listener should bind");
        let tcp_listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("TCP listener should bind");
        let tcp_address = tcp_listener.local_addr().expect("address should resolve");

        let relay_thread = thread::spawn(move || {
            let (public, _) = tcp_listener.accept().expect("TCP client should connect");
            let private = UnixStream::connect(unix_path).expect("Unix socket should connect");
            relay(public, private).expect("relay should finish");
        });
        let mut public = TcpStream::connect(tcp_address).expect("TCP endpoint should connect");
        let (mut private, _) = unix_listener.accept().expect("relay should connect");

        public
            .write_all(b"viewer-request")
            .expect("request should send");
        public
            .shutdown(Shutdown::Write)
            .expect("request should finish");
        let mut request = Vec::new();
        private
            .read_to_end(&mut request)
            .expect("request should arrive");
        assert_eq!(request, b"viewer-request");

        private
            .write_all(b"display-response")
            .expect("response should send");
        private
            .shutdown(Shutdown::Write)
            .expect("response should finish");
        let mut response = Vec::new();
        public
            .read_to_end(&mut response)
            .expect("response should arrive");
        assert_eq!(response, b"display-response");

        relay_thread.join().expect("relay thread should finish");
        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn malformed_bridge_status_is_rejected() {
        let root = temporary_directory("status");
        let path = root.join("launch.status");
        fs::write(&path, "READY but not really\n").expect("fixture should be written");
        assert!(read_status(&path).is_err());
        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn bridge_reports_an_early_viewer_exit_and_cleans_its_connection_file() {
        let root = temporary_directory("early-exit");
        let connection = root.join("viewer.vv");
        let status = root.join("launch.status");
        let arguments = [
            root.join("unused-vnc.sock").into_os_string(),
            connection.clone().into_os_string(),
            status.clone().into_os_string(),
            OsString::from("/bin/false"),
            OsString::from("win-dev"),
        ];

        assert!(bridge_command(&arguments).is_err());
        assert!(!connection.exists());
        match read_status(&status).expect("status should parse") {
            Some(BridgeStatus::Error(message)) => {
                assert!(message.contains("remote-viewer exited before connecting"));
            }
            _ => panic!("bridge should report an error"),
        }
        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }
}
