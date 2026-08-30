# ABOUTME: Prevents packaging from activating Voxkey or inserting it into the keyboard path.
# ABOUTME: These checks are offline and never start the daemon, IBus, or desktop portals.

from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def test_user_service_is_disabled_by_default():
    preset = (ROOT / "data/90-voxkey.preset").read_text()
    assert preset.strip() == "disable voxkey.service"


def test_user_service_never_restarts_after_failure():
    unit = (ROOT / "data/voxkey.service").read_text()
    assert "Restart=no" in unit
    assert "Restart=on-failure" not in unit


def test_user_service_requires_the_settings_lifecycle():
    unit = (ROOT / "data/voxkey.service").read_text()
    assert "ExecStart=/usr/bin/voxkey --settings-managed" in unit


def test_unsupported_ibus_implementation_is_absent():
    assert not (ROOT / "voxkey-ibus").exists()
    assert not (ROOT / "data/voxkey-ibus.xml").exists()


def test_main_rpm_does_not_install_ibus_or_dbus_activation():
    spec = (ROOT / "voxkey.spec").read_text()
    assert "voxkey-ibus-engine" not in spec
    assert "voxkey-ibus.xml" not in spec
    assert "io.github.hy26v.Voxkey.Daemon.service" not in spec
    assert "killall voxkey" not in spec


def test_main_rpm_owns_its_private_library_directory():
    spec = (ROOT / "voxkey.spec").read_text()
    assert "%dir %{_libdir}/voxkey" in spec


def test_rpm_and_release_ship_the_pinned_whisper_vad_model():
    spec = (ROOT / "voxkey.spec").read_text()
    workflow = (ROOT / ".github/workflows/release.yml").read_text()
    filename = "ggml-silero-v6.2.0.bin"
    checksum = "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987"

    assert f"%{{_datadir}}/voxkey/models/{filename}" in spec
    assert f"ggml-org/whisper-vad/resolve/main/{filename}" in workflow
    assert checksum in workflow
    assert "sha256sum --check --strict" in workflow


def test_shell_capsule_ends_before_insertion_and_has_only_dictation_controls():
    constants = (ROOT / "gnome-shell-extension" / "constants.js").read_text()
    capsule = (ROOT / "gnome-shell-extension" / "capsule.js").read_text()
    indicator_states = constants.split(
        "export const INDICATOR_STATES = new Set([", 1
    )[1].split("]);", 1)[0]
    capsule_builder = capsule.split(
        "        this._activityBars = [];", 1
    )[1].split("    _capsuleButton(", 1)[0]

    assert "STATE_CONNECTING" in indicator_states
    assert "STATE_RECORDING" in indicator_states
    assert "STATE_STREAMING" in indicator_states
    assert "STATE_TRANSCRIBING" in indicator_states
    assert "STATE_INJECTING" not in indicator_states
    assert "'Cancel'" in capsule_builder
    assert "'Finish'" in capsule_builder
    for post_dictation_action in ("'Retry'", "'Settings'", "'Dismiss'", "'Insert'"):
        assert post_dictation_action not in capsule_builder


def test_shell_capsule_preserves_capture_duration_during_transcription():
    capsule = (ROOT / "gnome-shell-extension" / "capsule.js").read_text()
    timer_sync = capsule.split("    syncElapsed(daemonState) {", 1)[1].split(
        "    resetElapsed() {", 1
    )[0]

    assert "daemonState === STATE_TRANSCRIBING" in timer_sync
    assert timer_sync.count("this._pauseElapsedTimer()") == 2
    assert "this.resetElapsed()" not in timer_sync
    assert "this.hide();" in capsule
    assert "this.resetElapsed();" in capsule.split("this.hide();", 1)[1]


def test_shell_labels_automatic_desktop_recovery_before_missing_permission():
    toggle = (ROOT / "gnome-shell-extension" / "toggle.js").read_text()
    human_state = toggle.split("    _humanState() {", 1)[1].split(
        "    _emptyPreviewText() {", 1
    )[0]

    assert "Restoring desktop access" in human_state
    assert human_state.index("STATE_RECOVERING") < human_state.index(
        "!this._portalConnected"
    )


def test_shell_menu_leaves_configuration_reload_in_expert_settings():
    toggle = (ROOT / "gnome-shell-extension" / "toggle.js").read_text()
    menu_builder = toggle.split("    _buildMenu() {", 1)[1].split(
        "    async _connectDaemon() {", 1
    )[0]
    settings = (ROOT / "voxkey-settings" / "src" / "window.rs").read_text()

    assert "Reload Configuration" not in menu_builder
    assert 'title("Reload configuration")' in settings


def test_shell_control_calls_have_a_finite_timeout():
    constants = (ROOT / "gnome-shell-extension" / "constants.js").read_text()
    toggle = (ROOT / "gnome-shell-extension" / "toggle.js").read_text()
    control_call = toggle.split("    async _callControl(", 1)[1].split(
        "    _cleanError(", 1
    )[0]

    assert "CONTROL_CALL_TIMEOUT_MS" in constants
    assert "Gio.DBusCallFlags.NONE,\n                    CONTROL_CALL_TIMEOUT_MS," in control_call
    assert "Gio.DBusCallFlags.NONE,\n                    -1," not in control_call


def test_release_does_not_stage_ibus_or_dbus_activation():
    workflow = (ROOT / ".github/workflows/release.yml").read_text()
    assert "voxkey-ibus-engine" not in workflow
    assert "voxkey-ibus.xml" not in workflow
    assert "io.github.hy26v.Voxkey.Daemon.service" not in workflow
    assert "cargo build --release --workspace" not in workflow


def test_runtime_never_uses_portal_keysym_injection():
    runtime = "\n".join(
        (ROOT / path).read_text()
        for path in ("src/desktop.rs", "src/eis.rs", "src/injector.rs")
    )
    assert "notify_keyboard_keysym" not in runtime
    assert "call_noreply" not in runtime
    assert "queue_tap_keysym" not in runtime
    assert "connect_to_eis" in runtime
    assert ".sync(1)" in runtime


def test_eis_connection_is_session_scoped_and_uses_explicit_keycodes():
    desktop = (ROOT / "src/desktop.rs").read_text()
    eis = (ROOT / "src/eis.rs").read_text()
    assert "EisSession::connect" in desktop
    assert desktop.count("connect_to_eis") == 1
    assert "run_worker" in desktop
    assert "keyboard.key(key, state)" in eis
    assert "stop_emulating" in eis
    assert "connection().disconnect()" in eis


def test_runtime_never_retries_partial_injection():
    runtime = "\n".join(
        (ROOT / path).read_text()
        for path in ("src/main.rs", "src/dbus.rs", "src/injector.rs")
    )
    assert "pending_injection" not in runtime
    assert "Retrying injection" not in runtime


def test_runtime_does_not_write_gnome_shortcut_dconf_directly():
    runtime = "\n".join(
        (ROOT / path).read_text()
        for path in ("src/main.rs", "src/dbus.rs", "src/shortcuts.rs")
    )
    assert "dconf" not in runtime


def test_production_daemon_has_no_ibus_control_bridge():
    runtime = "\n".join(
        (ROOT / path).read_text()
        for path in ("src/dbus.rs", "src/injector.rs", "src/streaming.rs")
    )
    assert "ibus_engine_active" not in runtime
    assert "set_ibus_engine_active" not in runtime


def test_ipc_and_settings_have_no_ibus_control_bridge():
    """The retired IBus engine must not leak through the shared IPC proxy or
    the settings GUI: the daemon no longer serves the bridge, and the GUI
    must not direct users to add the retired engine as an input source."""
    client = "\n".join(
        path.read_text()
        for path in (
            list((ROOT / "voxkey-ipc/src").glob("*.rs"))
            + list((ROOT / "voxkey-settings/src").glob("*.rs"))
        )
    )
    assert "ibus_engine_active" not in client
    assert "set_ibus_engine_active" not in client
    assert "Input Sources" not in client


def test_local_install_uses_rpm_instead_of_overwriting_package_files():
    script = (ROOT / "scripts/local-install.sh").read_text()

    assert "rpmbuild" in script
    assert "sudo dnf install" in script
    assert "--build-only" in script
    assert "sudo install" not in script
    assert "/usr/bin/voxkey" not in script
    assert "cargo install" not in script
    assert "killall" not in script
    assert "install -Dm644 data/io.github.hy26v.Voxkey.Daemon.service" not in script


def test_local_rpm_contains_the_same_pinned_vad_asset_as_the_release():
    script = (ROOT / "scripts/local-install.sh").read_text()

    assert "ggml-silero-v6.2.0.bin" in script
    assert "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987" in script
    assert "sha256sum --check --strict" in script
