// ABOUTME: Sends keyboard input through the session's compositor-tracked EIS connection.
// ABOUTME: Uses Mutter's keymap and explicit keycodes so every pressed key is tracked.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use futures_util::StreamExt;
use reis::PendingRequestResult;
use reis::ei::{self, keyboard::KeyState};
use reis::event::{Device, DeviceCapability, EiEvent, EiEventConverter};
use reis::tokio::EiEventStream;
use tokio::sync::oneshot;
use xkbcommon::xkb;

type DynError = Box<dyn std::error::Error + Send + Sync>;

const EIS_TIMEOUT: Duration = Duration::from_secs(2);
const XKB_EVDEV_OFFSET: u32 = 8;

#[derive(Clone, Copy, Default)]
struct ModifierState {
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
}

struct KeyPlan {
    modifiers: Vec<u32>,
    key: u32,
}

/// One EIS client and its compositor-provided virtual keyboard.
///
/// One instance lives for one RemoteDesktop portal session. Disconnecting it
/// makes Mutter release every key it recorded for this client.
pub struct EisSession {
    events: EiEventStream,
    converter: EiEventConverter,
    connection: reis::event::Connection,
    device: Option<Device>,
    keyboard: Option<ei::Keyboard>,
    keymap: Option<xkb::Keymap>,
    modifiers: ModifierState,
    sequence: u32,
    emulating: bool,
}

pub enum EisCommand {
    Inject {
        keysyms: Vec<i32>,
        delay: Duration,
        deadline: tokio::time::Instant,
        cancel: tokio::sync::watch::Receiver<bool>,
        result: oneshot::Sender<Result<(), InjectionFault>>,
    },
}

/// Why an injection did not happen.
#[derive(Debug)]
pub enum InjectionFault {
    /// The request was refused before the first key reached the compositor.
    /// The virtual keyboard is still healthy and the next dictation may retry.
    DeclinedBeforeWrite(String),
    /// A prefix reached the compositor before the remaining request became
    /// unsafe. The session is retired so a retry cannot race stale EIS state.
    Partial {
        message: String,
        inserted_keysyms: usize,
    },
    /// A deadline or explicit cancellation stopped the request between fully
    /// synchronized taps. The connection remains healthy.
    Interrupted {
        message: String,
        inserted_keysyms: usize,
    },
    /// The caller explicitly cancelled between fully synchronized taps.
    Cancelled { inserted_keysyms: usize },
    /// The EIS connection or protocol failed. The input session can no longer
    /// be trusted and has to be torn down. `inserted_keysyms` is conservative:
    /// it includes a tap whose release was flushed when its acknowledgement
    /// failed, because retrying that key could duplicate visible text.
    Session {
        message: String,
        inserted_keysyms: usize,
    },
}

impl InjectionFault {
    fn is_fatal(&self) -> bool {
        matches!(self, Self::Partial { .. } | Self::Session { .. })
    }

    pub fn inserted_keysyms(&self) -> usize {
        match self {
            Self::DeclinedBeforeWrite(_) => 0,
            Self::Partial {
                inserted_keysyms, ..
            }
            | Self::Interrupted {
                inserted_keysyms, ..
            }
            | Self::Cancelled { inserted_keysyms }
            | Self::Session {
                inserted_keysyms, ..
            } => *inserted_keysyms,
        }
    }
}

impl std::fmt::Display for InjectionFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeclinedBeforeWrite(message)
            | Self::Partial { message, .. }
            | Self::Interrupted { message, .. }
            | Self::Session { message, .. } => f.write_str(message),
            Self::Cancelled { .. } => f.write_str("EIS injection cancelled"),
        }
    }
}

impl std::error::Error for InjectionFault {}

impl EisSession {
    pub async fn connect(fd: std::os::fd::OwnedFd) -> Result<Self, DynError> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;

        let context = ei::Context::new(stream)?;
        let mut events = EiEventStream::new(context.clone())?;
        let handshake = tokio::time::timeout(
            EIS_TIMEOUT,
            reis::tokio::ei_handshake(&mut events, "voxkey", ei::handshake::ContextType::Sender),
        )
        .await
        .map_err(|_| "timed out during EIS handshake")??;

        let converter = EiEventConverter::new(&context, handshake);
        let connection = converter.connection().clone();
        let mut session = Self {
            events,
            converter,
            connection,
            device: None,
            keyboard: None,
            keymap: None,
            modifiers: ModifierState::default(),
            sequence: 1,
            emulating: false,
        };

        tokio::time::timeout(EIS_TIMEOUT, session.wait_until_ready())
            .await
            .map_err(|_| "timed out waiting for an EIS keyboard")??;
        session.sync().await?;
        Ok(session)
    }

    async fn wait_until_ready(&mut self) -> Result<(), DynError> {
        while !self.emulating {
            self.receive_protocol_event().await?;
        }
        Ok(())
    }

    async fn receive_protocol_event(&mut self) -> Result<(), DynError> {
        let item = self.events.next().await.ok_or("EIS connection closed")??;
        match item {
            PendingRequestResult::Request(event) => self.converter.handle_event(event)?,
            PendingRequestResult::ParseError(error) => return Err(error.into()),
            PendingRequestResult::InvalidObject(object) => {
                return Err(format!("EIS referenced invalid object {object:?}").into());
            }
        }

        let mut events = Vec::new();
        while let Some(event) = self.converter.next_event() {
            events.push(event);
        }
        for event in events {
            self.handle_event(event)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: EiEvent) -> Result<(), DynError> {
        match event {
            EiEvent::SeatAdded(event) => {
                event
                    .seat
                    .bind_capabilities(DeviceCapability::Keyboard.into());
                self.connection.flush()?;
            }
            EiEvent::DeviceAdded(event)
                if event.device.has_capability(DeviceCapability::Keyboard) =>
            {
                let protocol_keymap = event
                    .device
                    .keymap()
                    .ok_or("EIS keyboard did not provide an XKB keymap")?;
                if protocol_keymap.type_ != ei::keyboard::KeymapType::Xkb {
                    return Err("EIS keyboard provided an unsupported keymap type".into());
                }
                let fd = protocol_keymap.fd.as_fd().try_clone_to_owned()?;
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        protocol_keymap.size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::COMPILE_NO_FLAGS,
                    )
                }?
                .ok_or("failed to compile the compositor's EIS keymap")?;
                self.keyboard = event.device.interface::<ei::Keyboard>();
                self.keymap = Some(keymap);
                self.device = Some(event.device);
            }
            EiEvent::DeviceResumed(event)
                if self.device.as_ref() == Some(&event.device) && !self.emulating =>
            {
                event
                    .device
                    .device()
                    .start_emulating(event.serial, self.sequence);
                self.sequence = self.sequence.wrapping_add(1);
                self.connection.flush()?;
                self.emulating = true;
            }
            EiEvent::KeyboardModifiers(event) if self.device.as_ref() == Some(&event.device) => {
                self.modifiers = ModifierState {
                    depressed: event.depressed,
                    latched: event.latched,
                    locked: event.locked,
                    group: event.group,
                };
            }
            EiEvent::DevicePaused(event) if self.device.as_ref() == Some(&event.device) => {
                self.modifiers = ModifierState::default();
                self.emulating = false;
                return Err("EIS keyboard was paused".into());
            }
            EiEvent::DeviceRemoved(event) if self.device.as_ref() == Some(&event.device) => {
                self.emulating = false;
                return Err("EIS keyboard was removed".into());
            }
            EiEvent::Disconnected(event) => {
                return Err(format!("EIS disconnected: {event:?}").into());
            }
            _ => {}
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn type_keysyms(
        &mut self,
        keysyms: &[i32],
        delay: Duration,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), InjectionFault> {
        let (_cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        self.type_keysyms_until(
            keysyms,
            delay,
            shutdown,
            &mut cancel_rx,
            tokio::time::Instant::now() + crate::deadline::INJECTION_OPERATION,
        )
        .await
    }

    async fn type_keysyms_until(
        &mut self,
        keysyms: &[i32],
        delay: Duration,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        request_cancel: &mut tokio::sync::watch::Receiver<bool>,
        deadline: tokio::time::Instant,
    ) -> Result<(), InjectionFault> {
        if *request_cancel.borrow() {
            return Err(InjectionFault::Cancelled {
                inserted_keysyms: 0,
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(InjectionFault::Interrupted {
                message: "EIS injection deadline expired before typing".into(),
                inserted_keysyms: 0,
            });
        }
        if *shutdown.borrow() {
            return Err(InjectionFault::Session {
                message: "EIS injection cancelled".into(),
                inserted_keysyms: 0,
            });
        }
        // Drain all compositor state that predates this batch before deciding
        // whether the current layout and modifier state are safe to use.
        self.sync().await.map_err(|error| InjectionFault::Session {
            message: error.to_string(),
            inserted_keysyms: 0,
        })?;
        if self.modifiers.depressed != 0 || self.modifiers.latched != 0 {
            return Err(InjectionFault::DeclinedBeforeWrite(
                "refusing to inject while a physical modifier is pressed or latched".into(),
            ));
        }

        // Plan the whole batch before sending its first key. Unsupported text
        // therefore cannot leave a partially typed transcript.
        let plans = keysyms
            .iter()
            .copied()
            .map(|keysym| self.plan_keysym(xkb::Keysym::new(keysym as u32)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| InjectionFault::DeclinedBeforeWrite(error.to_string()))?;

        for (index, plan) in plans.iter().enumerate() {
            if *request_cancel.borrow() {
                return Err(InjectionFault::Cancelled {
                    inserted_keysyms: index,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(InjectionFault::Interrupted {
                    message: "EIS injection exceeded its total deadline".into(),
                    inserted_keysyms: index,
                });
            }
            if *shutdown.borrow() {
                return Err(InjectionFault::Session {
                    message: "EIS injection cancelled".into(),
                    inserted_keysyms: index,
                });
            }
            if index > 0 && !delay.is_zero() {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = tokio::time::sleep_until(deadline) => {
                        return Err(InjectionFault::Interrupted {
                            message: "EIS injection exceeded its total deadline".into(),
                            inserted_keysyms: index,
                        });
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Err(InjectionFault::Session {
                                message: "EIS injection cancelled".into(),
                                inserted_keysyms: index,
                            });
                        }
                    }
                    changed = request_cancel.changed() => {
                        if changed.is_err() || *request_cancel.borrow() {
                            return Err(InjectionFault::Cancelled {
                                inserted_keysyms: index,
                            });
                        }
                    }
                }

                // Protocol events are not processed while this worker is
                // sleeping. Synchronize again after the delay so a physical
                // modifier pressed in that window cannot combine with the
                // next virtual key.
                self.sync().await.map_err(|error| InjectionFault::Session {
                    message: error.to_string(),
                    inserted_keysyms: index,
                })?;
            }
            if self.modifiers.depressed != 0 || self.modifiers.latched != 0 {
                let message =
                    "refusing to continue injection after a modifier became active".into();
                return Err(if index == 0 {
                    InjectionFault::DeclinedBeforeWrite(message)
                } else {
                    InjectionFault::Partial {
                        message,
                        inserted_keysyms: index,
                    }
                });
            }
            self.tap_plan(plan)
                .await
                .map_err(|error| InjectionFault::Session {
                    message: error.to_string(),
                    inserted_keysyms: index + 1,
                })?;
        }
        Ok(())
    }

    async fn tap_plan(&mut self, plan: &KeyPlan) -> Result<(), DynError> {
        let mut pressed = Vec::with_capacity(plan.modifiers.len() + 1);

        for key in plan.modifiers.iter().copied() {
            self.send_key(key, KeyState::Press)?;
            pressed.push(key);
        }
        self.send_key(plan.key, KeyState::Press)?;
        pressed.push(plan.key);

        while let Some(key) = pressed.pop() {
            self.send_key(key, KeyState::Released)?;
        }

        self.connection.flush()?;
        self.sync().await
    }

    fn send_key(&mut self, key: u32, state: KeyState) -> Result<(), DynError> {
        if !self.emulating {
            return Err("EIS keyboard is not emulating".into());
        }
        let keyboard = self.keyboard.as_ref().ok_or("EIS keyboard unavailable")?;
        let device = self.device.as_ref().ok_or("EIS device unavailable")?;
        if !keyboard.is_alive() || !device.device().is_alive() {
            return Err("EIS keyboard was destroyed".into());
        }

        keyboard.key(key, state);
        device
            .device()
            .frame(self.connection.serial(), monotonic_microseconds());
        Ok(())
    }

    fn plan_keysym(&self, target: xkb::Keysym) -> Result<KeyPlan, DynError> {
        let keymap = self.keymap.as_ref().ok_or("EIS keymap unavailable")?;
        plan_keysym(keymap, self.modifiers, target)
    }

    async fn sync(&mut self) -> Result<(), DynError> {
        let callback = self.connection.connection().sync(1);
        let (done_tx, mut done_rx) = oneshot::channel();
        self.converter.add_callback_handler(callback, move |_| {
            let _ = done_tx.send(());
        });
        self.connection.flush()?;

        tokio::time::timeout(EIS_TIMEOUT, async {
            loop {
                tokio::select! {
                    result = &mut done_rx => {
                        result.map_err(|_| "EIS synchronization callback was dropped")?;
                        return Ok::<(), DynError>(());
                    }
                    result = self.receive_protocol_event() => result?,
                }
            }
        })
        .await
        .map_err(|_| "timed out synchronizing EIS keyboard events")??;
        Ok(())
    }

    pub fn shutdown(&mut self) {
        if self.emulating {
            if let Some(device) = &self.device {
                device.device().stop_emulating(self.connection.serial());
            }
            self.emulating = false;
        }
        self.connection.connection().disconnect();
        let _ = self.connection.flush();
    }
}

/// Own an EIS connection until its RemoteDesktop session is closed.
///
/// The portal permits exactly one ConnectToEIS call per session. This loop
/// keeps that connection neutral while idle, processes compositor state
/// changes, and disconnects permanently after any protocol or injection error.
pub async fn run_worker(
    mut session: EisSession,
    mut commands: tokio::sync::mpsc::Receiver<EisCommand>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> bool {
    let mut failed = false;
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(EisCommand::Inject { keysyms, delay, deadline, mut cancel, result }) => {
                        match session.type_keysyms_until(
                            &keysyms,
                            delay,
                            &mut shutdown,
                            &mut cancel,
                            deadline,
                        ).await {
                            Ok(()) => {
                                let _ = result.send(Ok(()));
                            }
                            Err(fault) => {
                                // A planned stop between synchronized taps is
                                // also safe; partial/protocol faults retire it.
                                let fatal = fault.is_fatal();
                                if !fatal {
                                    tracing::warn!("Stopped injection without damaging the session: {fault}");
                                }
                                let _ = result.send(Err(fault));
                                if fatal {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            result = session.receive_protocol_event() => {
                if let Err(error) = result {
                    tracing::error!("EIS connection stopped: {error}");
                    failed = true;
                    break;
                }
            }
        }
    }

    session.shutdown();
    failed
}

fn plan_keysym(
    keymap: &xkb::Keymap,
    modifiers: ModifierState,
    target: xkb::Keysym,
) -> Result<KeyPlan, DynError> {
    let group = modifiers.group;
    let modifier_candidates = safe_modifier_keys(keymap, group);

    let combinations = modifier_combinations(&modifier_candidates);

    for combination in combinations {
        let depressed = combination
            .iter()
            .fold(0, |mask, (_, modifier_mask)| mask | modifier_mask);
        let mut state = xkb::State::new(keymap);
        state.update_mask(depressed, 0, modifiers.locked, 0, 0, group);

        for raw_keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let keycode = xkb::Keycode::new(raw_keycode);
            if state.key_get_one_sym(keycode) != target || raw_keycode < XKB_EVDEV_OFFSET {
                continue;
            }
            let evdev_key = raw_keycode - XKB_EVDEV_OFFSET;
            if combination.iter().any(|(key, _)| *key == evdev_key) {
                continue;
            }
            return Ok(KeyPlan {
                modifiers: combination.iter().map(|(key, _)| *key).collect(),
                key: evdev_key,
            });
        }
    }

    Err(format!(
        "the keyboard layout in use cannot type {}",
        describe_keysym(target)
    )
    .into())
}

fn modifier_combinations(candidates: &[(u32, u32)]) -> Vec<Vec<(u32, u32)>> {
    let mut combinations = vec![Vec::new()];
    for candidate in candidates.iter().copied() {
        let additions = combinations
            .iter()
            .filter(|combination| {
                !combination
                    .iter()
                    .any(|(_, modifier_mask)| *modifier_mask == candidate.1)
            })
            .cloned()
            .map(|mut combination| {
                combination.push(candidate);
                combination
            })
            .collect::<Vec<_>>();
        combinations.extend(additions);
    }
    combinations.sort_by_key(Vec::len);
    combinations
}

/// Name a keysym the way a user can recognise it, so a refusal explains which
/// character stopped the dictation rather than quoting a protocol number.
fn describe_keysym(target: xkb::Keysym) -> String {
    let text = xkb::keysym_to_utf8(target);
    let character = text.trim_end_matches('\0');
    if character.is_empty() {
        return format!("keysym 0x{:x}", target.raw());
    }
    match character.chars().next() {
        Some(single) => format!("'{character}' (U+{:04X})", single as u32),
        None => format!("keysym 0x{:x}", target.raw()),
    }
}

fn safe_modifier_keys(keymap: &xkb::Keymap, group: u32) -> Vec<(u32, u32)> {
    const SAFE_MODIFIERS: [u32; 4] = [
        xkb::keysyms::KEY_Shift_L,
        xkb::keysyms::KEY_Shift_R,
        xkb::keysyms::KEY_ISO_Level3_Shift,
        xkb::keysyms::KEY_ISO_Level5_Shift,
    ];

    let mut result = Vec::new();
    for raw_keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        if raw_keycode < XKB_EVDEV_OFFSET {
            continue;
        }
        let keycode = xkb::Keycode::new(raw_keycode);
        let syms = keymap.key_get_syms_by_level(keycode, group, 0);
        if !syms
            .iter()
            .any(|keysym| SAFE_MODIFIERS.contains(&keysym.raw()))
        {
            continue;
        }

        let mut state = xkb::State::new(keymap);
        state.update_key(keycode, xkb::KeyDirection::Down);
        let mask = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        if mask != 0 && !result.iter().any(|(_, existing)| *existing == mask) {
            result.push((raw_keycode - XKB_EVDEV_OFFSET, mask));
        }
    }
    result
}

fn monotonic_microseconds() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    (now.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add((now.tv_nsec as u64) / 1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use calloop::PostAction;
    use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
    use reis::eis;
    use reis::request::{
        Connection as ServerConnection, Device as ServerDevice,
        DeviceCapability as ServerCapability, EisRequest,
    };

    pub fn us_keymap() -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
            .unwrap()
    }

    fn eurkey_keymap() -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&context, "", "", "eu", "", None, xkb::COMPILE_NO_FLAGS)
            .unwrap()
    }

    #[test]
    fn safe_modifiers_only_include_depressed_modifier_keys() {
        let modifiers = safe_modifier_keys(&us_keymap(), 0);
        assert!(!modifiers.is_empty());
        assert!(modifiers.iter().all(|(_, mask)| *mask != 0));
    }

    #[test]
    fn planner_considers_three_modifier_combinations() {
        let candidates = [(10, 1), (20, 2), (30, 4)];

        let combinations = modifier_combinations(&candidates);

        assert!(combinations.contains(&candidates.to_vec()));
    }

    #[test]
    fn lowercase_uses_plain_evdev_keycode() {
        let plan = plan_keysym(
            &us_keymap(),
            ModifierState::default(),
            xkb::Keysym::from_char('a'),
        )
        .unwrap();
        assert_eq!(plan.key, 30);
        assert!(plan.modifiers.is_empty());
    }

    #[test]
    fn uppercase_uses_explicit_tracked_shift_keycode() {
        let plan = plan_keysym(
            &us_keymap(),
            ModifierState::default(),
            xkb::Keysym::from_char('A'),
        )
        .unwrap();
        assert_eq!(plan.key, 30);
        assert_eq!(plan.modifiers.len(), 1);
    }

    #[test]
    fn unmapped_character_is_rejected_instead_of_using_keysym_injection() {
        let result = plan_keysym(
            &us_keymap(),
            ModifierState::default(),
            xkb::Keysym::from_char('\u{1f642}'),
        );
        assert!(result.is_err());
    }

    /// The refusal reaches the user as an error message, so it has to name the
    /// character that could not be typed rather than a protocol number.
    #[test]
    fn a_refusal_names_the_character_it_could_not_type() {
        let error = plan_keysym(
            &us_keymap(),
            ModifierState::default(),
            // U+2019, the apostrophe transcribers put in "don't".
            xkb::utf32_to_keysym(0x2019),
        )
        .err()
        .expect("a curly apostrophe is not typeable on a US layout")
        .to_string();

        assert!(error.contains('\u{2019}'), "{error}");
        assert!(error.contains("U+2019"), "{error}");
        assert!(error.contains("layout"), "{error}");
    }

    #[test]
    fn configured_eurkey_layout_can_plan_common_dictation_text() {
        let keymap = eurkey_keymap();
        for character in "Hello, café! Über niño €".chars() {
            let target = xkb::utf32_to_keysym(character as u32);
            plan_keysym(&keymap, ModifierState::default(), target)
                .unwrap_or_else(|error| panic!("EurKEY could not plan {character:?}: {error}"));
        }
    }

    #[test]
    fn caps_lock_state_is_included_when_planning_case() {
        let keymap = us_keymap();
        let caps_keycode = (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .map(xkb::Keycode::new)
            .find(|keycode| {
                keymap
                    .key_get_syms_by_level(*keycode, 0, 0)
                    .iter()
                    .any(|symbol| symbol.raw() == xkb::keysyms::KEY_Caps_Lock)
            })
            .unwrap();
        let mut caps_state = xkb::State::new(&keymap);
        caps_state.update_key(caps_keycode, xkb::KeyDirection::Down);
        let modifiers = ModifierState {
            locked: caps_state.serialize_mods(xkb::STATE_MODS_LOCKED),
            ..ModifierState::default()
        };
        assert_ne!(modifiers.locked, 0);

        let lowercase = plan_keysym(&keymap, modifiers, xkb::Keysym::from_char('a')).unwrap();
        let uppercase = plan_keysym(&keymap, modifiers, xkb::Keysym::from_char('A')).unwrap();
        assert_eq!(lowercase.modifiers.len(), 1);
        assert!(uppercase.modifiers.is_empty());
    }

    type RecordedKeys = Arc<Mutex<Vec<(u32, eis::keyboard::KeyState)>>>;

    struct MockServerState {
        connection: Option<ServerConnection>,
        keyboard_device: Option<ServerDevice>,
        first_release_at: Option<std::time::Instant>,
        modifier_sent: bool,
    }

    /// A compositor-side EIS server offering one US-layout virtual keyboard,
    /// recording every key it is asked to press or release. Returns the client
    /// end of the socket, the recording, and the server thread.
    fn spawn_mock_eis_server(
        modifier_after_first_tap: bool,
    ) -> (UnixStream, RecordedKeys, std::thread::JoinHandle<()>) {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        client_socket.set_nonblocking(true).unwrap();
        server_socket.set_nonblocking(true).unwrap();

        let keymap_text = us_keymap().get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let received: RecordedKeys = Arc::new(Mutex::new(Vec::new()));
        let server_received = received.clone();

        let server = std::thread::spawn(move || {
            let context = eis::Context::new(server_socket).unwrap();
            let source = EisRequestSource::new(context, 1);
            let mut event_loop = calloop::EventLoop::<MockServerState>::try_new().unwrap();
            let handle = event_loop.handle();
            let disconnected = Arc::new(AtomicBool::new(false));
            let callback_disconnected = disconnected.clone();
            let mut state = MockServerState {
                connection: None,
                keyboard_device: None,
                first_release_at: None,
                modifier_sent: false,
            };

            handle
                .insert_source(source, move |event, connection, state| {
                    match event.unwrap() {
                        EisRequestSourceEvent::Connected => {
                            state.connection = Some(connection.clone());
                            let _ = connection
                                .add_seat(Some("mock seat"), ServerCapability::Keyboard.into());
                        }
                        EisRequestSourceEvent::Request(EisRequest::Bind(request)) => {
                            let mut keymap_file = tempfile::tempfile().unwrap();
                            keymap_file.write_all(keymap_text.as_bytes()).unwrap();
                            let keymap_size = keymap_text.len() as u32;
                            let device = request.seat.add_device(
                                Some("mock keyboard"),
                                eis::device::DeviceType::Virtual,
                                ServerCapability::Keyboard.into(),
                                |device| {
                                    device.interface::<eis::Keyboard>().unwrap().keymap(
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
                            server_received
                                .lock()
                                .unwrap()
                                .push((event.key, event.state));
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
                })
                .unwrap();

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !disconnected.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                event_loop
                    .dispatch(Some(Duration::from_millis(5)), &mut state)
                    .unwrap();
                let should_send_modifier = modifier_after_first_tap
                    && !state.modifier_sent
                    && state
                        .first_release_at
                        .is_some_and(|at| at.elapsed() >= Duration::from_millis(20));
                if should_send_modifier {
                    let keyboard = state
                        .keyboard_device
                        .as_ref()
                        .and_then(|device| device.interface::<eis::Keyboard>())
                        .expect("mock keyboard must still be available");
                    keyboard.modifiers(10_000, 1, 0, 0, 0);
                    state
                        .connection
                        .as_ref()
                        .expect("mock connection must still be available")
                        .flush()
                        .unwrap();
                    state.modifier_sent = true;
                }
            }
            assert!(
                disconnected.load(Ordering::Acquire),
                "mock EIS client did not disconnect"
            );
            if modifier_after_first_tap {
                assert!(state.modifier_sent, "mock modifier event was never sent");
            }
        });

        (client_socket, received, server)
    }

    /// Speech models routinely produce typographic punctuation such as the
    /// apostrophe in "don't", which a plain US layout cannot type. Refusing
    /// that text must not take down the input session: the connection is
    /// healthy and the next dictation has to keep working.
    #[test]
    fn text_the_layout_cannot_type_leaves_the_session_usable() {
        let (client_socket, received, server) = spawn_mock_eis_server(false);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        // The worker owns non-Send protocol objects, so it runs on this thread
        // exactly as the daemon runs it on its dedicated EIS thread.
        let local = tokio::task::LocalSet::new();
        runtime.block_on(local.run_until(async {
            let session = EisSession::connect(client_socket.into()).await.unwrap();
            let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(2);
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let worker = tokio::task::spawn_local(run_worker(session, commands_rx, shutdown_rx));

            let (result_tx, result_rx) = oneshot::channel();
            let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
            commands_tx
                .send(EisCommand::Inject {
                    // U+2019, the apostrophe transcribers put in "don't".
                    keysyms: vec![xkb::utf32_to_keysym(0x2019).raw() as i32],
                    delay: Duration::ZERO,
                    deadline: tokio::time::Instant::now() + crate::deadline::INJECTION_OPERATION,
                    cancel,
                    result: result_tx,
                })
                .await
                .unwrap();
            let refusal = result_rx
                .await
                .expect("the worker must answer instead of dying")
                .expect_err("untypeable text cannot be reported as typed");
            assert!(
                refusal.to_string().contains("layout"),
                "unexpected refusal: {refusal}"
            );

            let (result_tx, result_rx) = oneshot::channel();
            let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
            commands_tx
                .send(EisCommand::Inject {
                    keysyms: vec![xkb::Keysym::from_char('a').raw() as i32],
                    delay: Duration::ZERO,
                    deadline: tokio::time::Instant::now() + crate::deadline::INJECTION_OPERATION,
                    cancel,
                    result: result_tx,
                })
                .await
                .unwrap();
            result_rx
                .await
                .expect("the worker must still be running")
                .expect("the next dictation must still be typed");

            let _ = shutdown_tx.send(true);
            assert!(
                !worker.await.unwrap(),
                "text the layout cannot type must not mark the input session as failed"
            );
        }));
        server.join().unwrap();

        let events = received.lock().unwrap();
        assert_eq!(
            *events,
            [
                (30, eis::keyboard::KeyState::Press),
                (30, eis::keyboard::KeyState::Released),
            ],
            "only the typeable text should have reached the compositor"
        );
    }

    #[test]
    fn modifier_pressed_during_typing_delay_blocks_the_next_key() {
        let (client_socket, received, server) = spawn_mock_eis_server(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut session = EisSession::connect(client_socket.into()).await.unwrap();
            let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
            let error = session
                .type_keysyms(
                    &[
                        xkb::Keysym::from_char('a').raw() as i32,
                        xkb::Keysym::from_char('b').raw() as i32,
                    ],
                    Duration::from_millis(150),
                    &mut shutdown_rx,
                )
                .await
                .expect_err("a physical modifier must stop the remaining batch");
            assert!(
                matches!(
                    &error,
                    InjectionFault::Partial {
                        inserted_keysyms: 1,
                        ..
                    }
                ),
                "a one-character prefix must be reported explicitly: {error:?}"
            );
            assert!(error.to_string().contains("modifier"), "{error}");
            session.shutdown();
        });
        server.join().unwrap();

        assert_eq!(
            *received.lock().unwrap(),
            [
                (30, eis::keyboard::KeyState::Press),
                (30, eis::keyboard::KeyState::Released),
            ],
            "the key after the delayed modifier event must not be sent"
        );
    }

    #[test]
    fn whole_injection_deadline_stops_between_balanced_taps() {
        let (client_socket, received, server) = spawn_mock_eis_server(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut session = EisSession::connect(client_socket.into()).await.unwrap();
            let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
            let (_cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
            let error = session
                .type_keysyms_until(
                    &[
                        xkb::Keysym::from_char('a').raw() as i32,
                        xkb::Keysym::from_char('b').raw() as i32,
                    ],
                    Duration::from_secs(1),
                    &mut shutdown_rx,
                    &mut cancel_rx,
                    tokio::time::Instant::now() + Duration::from_millis(50),
                )
                .await
                .expect_err("the whole-operation deadline must stop the second tap");
            assert!(matches!(
                error,
                InjectionFault::Interrupted {
                    inserted_keysyms: 1,
                    ..
                }
            ));
            session.shutdown();
        });
        server.join().unwrap();

        assert_eq!(
            *received.lock().unwrap(),
            [
                (30, eis::keyboard::KeyState::Press),
                (30, eis::keyboard::KeyState::Released),
            ]
        );
    }

    #[test]
    fn explicit_cancellation_stops_between_balanced_taps() {
        let (client_socket, received, server) = spawn_mock_eis_server(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut session = EisSession::connect(client_socket.into()).await.unwrap();
            let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
            let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel_tx.send_replace(true);
            });

            let error = session
                .type_keysyms_until(
                    &[
                        xkb::Keysym::from_char('a').raw() as i32,
                        xkb::Keysym::from_char('b').raw() as i32,
                    ],
                    Duration::from_secs(1),
                    &mut shutdown_rx,
                    &mut cancel_rx,
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
                .expect_err("cancellation must preempt the inter-key delay");
            assert!(matches!(
                error,
                InjectionFault::Cancelled {
                    inserted_keysyms: 1
                }
            ));
            session.shutdown();
        });
        server.join().unwrap();

        assert_eq!(
            *received.lock().unwrap(),
            [
                (30, eis::keyboard::KeyState::Press),
                (30, eis::keyboard::KeyState::Released),
            ]
        );
    }

    #[test]
    fn eis_protocol_cancellation_leaves_a_balanced_explicit_shift_sequence() {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        client_socket.set_nonblocking(true).unwrap();
        server_socket.set_nonblocking(true).unwrap();

        let keymap_text = us_keymap().get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let received = Arc::new(Mutex::new(Vec::new()));
        let server_received = received.clone();

        let server = std::thread::spawn(move || {
            let context = eis::Context::new(server_socket).unwrap();
            let source = EisRequestSource::new(context, 1);
            let mut event_loop = calloop::EventLoop::<()>::try_new().unwrap();
            let handle = event_loop.handle();
            let mut keyboard_device: Option<reis::request::Device> = None;
            let disconnected = Arc::new(AtomicBool::new(false));
            let callback_disconnected = disconnected.clone();

            handle
                .insert_source(source, move |event, connection, _state| {
                    match event.unwrap() {
                        EisRequestSourceEvent::Connected => {
                            let _ = connection
                                .add_seat(Some("mock seat"), ServerCapability::Keyboard.into());
                        }
                        EisRequestSourceEvent::Request(EisRequest::Bind(request)) => {
                            let mut keymap_file = tempfile::tempfile().unwrap();
                            keymap_file.write_all(keymap_text.as_bytes()).unwrap();
                            let keymap_size = keymap_text.len() as u32;
                            let device = request.seat.add_device(
                                Some("mock keyboard"),
                                eis::device::DeviceType::Virtual,
                                ServerCapability::Keyboard.into(),
                                |device| {
                                    device.interface::<eis::Keyboard>().unwrap().keymap(
                                        eis::keyboard::KeymapType::Xkb,
                                        keymap_size,
                                        keymap_file.as_fd(),
                                    );
                                },
                            );
                            device.resumed();
                            keyboard_device = Some(device);
                        }
                        EisRequestSourceEvent::Request(EisRequest::KeyboardKey(event)) => {
                            server_received
                                .lock()
                                .unwrap()
                                .push((event.key, event.state));
                        }
                        EisRequestSourceEvent::Request(EisRequest::Disconnect) => {
                            callback_disconnected.store(true, Ordering::Release);
                            keyboard_device.take();
                            return Ok(PostAction::Remove);
                        }
                        _ => {}
                    }
                    connection.flush()?;
                    Ok(PostAction::Continue)
                })
                .unwrap();

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !disconnected.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                event_loop
                    .dispatch(Some(Duration::from_millis(50)), &mut ())
                    .unwrap();
            }
            assert!(
                disconnected.load(Ordering::Acquire),
                "mock EIS client did not disconnect"
            );
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut session = EisSession::connect(client_socket.into()).await.unwrap();
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
            let cancellation_events = received.clone();
            tokio::spawn(async move {
                while cancellation_events.lock().unwrap().len() < 4 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let _ = shutdown_tx.send(true);
            });
            let error = session
                .type_keysyms(
                    &[
                        xkb::Keysym::from_char('A').raw() as i32,
                        xkb::Keysym::from_char('A').raw() as i32,
                    ],
                    Duration::from_secs(60),
                    &mut shutdown_rx,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            session.shutdown();
        });
        server.join().unwrap();

        let events = received.lock().unwrap();
        assert_eq!(events.len(), 4, "unexpected EIS key sequence: {events:?}");
        let shift = events[0].0;
        assert!(shift == 42 || shift == 54, "expected a Shift keycode");
        assert_eq!(events[0].1, eis::keyboard::KeyState::Press);
        assert_eq!(events[1], (30, eis::keyboard::KeyState::Press));
        assert_eq!(events[2], (30, eis::keyboard::KeyState::Released));
        assert_eq!(events[3], (shift, eis::keyboard::KeyState::Released));
    }
}
