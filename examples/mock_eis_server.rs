// ABOUTME: Provides a compositor-side EIS keyboard for the private integration-test portal.
// ABOUTME: Accepts one inherited Unix socket and acknowledges injected key events without a desktop.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use calloop::PostAction;
use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
use reis::eis;
use reis::request::{
    Connection as ServerConnection, Device as ServerDevice, DeviceCapability as ServerCapability,
    EisRequest,
};
use xkbcommon::xkb;

struct ServerState {
    connection: Option<ServerConnection>,
    keyboard_device: Option<ServerDevice>,
    event_log: Option<File>,
    first_release_at: Option<std::time::Instant>,
    modifier_after_first_tap: bool,
    modifier_sent: bool,
}

fn us_keymap() -> Result<xkb::Keymap, Box<dyn std::error::Error>> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
        .ok_or_else(|| "could not build the mock US keymap".into())
}

fn inherited_socket() -> Result<UnixStream, Box<dyn std::error::Error>> {
    let raw: RawFd = std::env::args()
        .nth(1)
        .ok_or("usage: mock_eis_server <inherited-fd>")?
        .parse()?;
    // SAFETY: the private portal passes this descriptor through `pass_fds`
    // and transfers ownership of the child-side socket to this process.
    let socket = unsafe { UnixStream::from_raw_fd(raw) };
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = inherited_socket()?;
    let event_log = std::env::var_os("VOXKEY_TEST_EIS_EVENT_LOG")
        .map(|path| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
        })
        .transpose()?;
    let modifier_after_first_tap = std::env::var_os("VOXKEY_TEST_EIS_MODIFIER_AFTER_FIRST_TAP")
        .as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let keymap_text = us_keymap()?.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let context = eis::Context::new(socket)?;
    let source = EisRequestSource::new(context, 1);
    let mut event_loop = calloop::EventLoop::<ServerState>::try_new()?;
    let disconnected = Arc::new(AtomicBool::new(false));
    let callback_disconnected = disconnected.clone();
    let mut state = ServerState {
        connection: None,
        keyboard_device: None,
        event_log,
        first_release_at: None,
        modifier_after_first_tap,
        modifier_sent: false,
    };

    event_loop
        .handle()
        .insert_source(source, move |event, connection, state| {
            match event.expect("mock EIS request should parse") {
                EisRequestSourceEvent::Connected => {
                    state.connection = Some(connection.clone());
                    let _ = connection
                        .add_seat(Some("Voxkey test seat"), ServerCapability::Keyboard.into());
                }
                EisRequestSourceEvent::Request(EisRequest::Bind(request)) => {
                    let mut keymap_file = tempfile::tempfile().expect("create keymap file");
                    keymap_file
                        .write_all(keymap_text.as_bytes())
                        .expect("write keymap file");
                    let keymap_size =
                        u32::try_from(keymap_text.len()).expect("mock keymap size should fit u32");
                    let device = request.seat.add_device(
                        Some("Voxkey test keyboard"),
                        eis::device::DeviceType::Virtual,
                        ServerCapability::Keyboard.into(),
                        |device| {
                            device
                                .interface::<eis::Keyboard>()
                                .expect("keyboard interface")
                                .keymap(
                                    eis::keyboard::KeymapType::Xkb,
                                    keymap_size,
                                    keymap_file.as_fd(),
                                );
                        },
                    );
                    device.resumed();
                    state.keyboard_device = Some(device);
                }
                EisRequestSourceEvent::Request(EisRequest::KeyboardKey(event)) => {
                    if let Some(log) = state.event_log.as_mut() {
                        writeln!(log, "{} {:?}", event.key, event.state)?;
                        log.flush()?;
                    }
                    if event.state == eis::keyboard::KeyState::Released
                        && state.first_release_at.is_none()
                    {
                        state.first_release_at = Some(std::time::Instant::now());
                    }
                }
                EisRequestSourceEvent::Request(EisRequest::Disconnect) => {
                    callback_disconnected.store(true, Ordering::Release);
                    state.keyboard_device.take();
                    return Ok(PostAction::Remove);
                }
                _ => {}
            }
            connection.flush()?;
            Ok(PostAction::Continue)
        })?;

    while !disconnected.load(Ordering::Acquire) {
        event_loop.dispatch(Some(Duration::from_millis(25)), &mut state)?;
        let should_send_modifier = state.modifier_after_first_tap
            && !state.modifier_sent
            && state
                .first_release_at
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(10));
        if should_send_modifier {
            let keyboard = state
                .keyboard_device
                .as_ref()
                .and_then(|device| device.interface::<eis::Keyboard>())
                .ok_or("mock keyboard disappeared before modifier injection")?;
            keyboard.modifiers(10_000, 1, 0, 0, 0);
            state
                .connection
                .as_ref()
                .ok_or("mock EIS connection disappeared before modifier injection")?
                .flush()?;
            state.modifier_sent = true;
        }
    }
    Ok(())
}
