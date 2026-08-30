# ABOUTME: Verifies D-Bus settings merge into the user's latest TOML document.
# ABOUTME: Comments, unknown settings, and unrelated live edits must survive a save.

import os
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.mark.asyncio
async def test_setting_change_preserves_latest_manual_toml_edits(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    proxy = dbus_session.get_proxy_object(
        BUS_NAME,
        OBJECT_PATH,
        await safe_introspect(dbus_session, BUS_NAME, OBJECT_PATH),
    ).get_interface(INTERFACE)

    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    config_path.write_text(
        """# Keep this user documentation.
future_root_setting = "untouched"

[shortcut]
trigger = "<Super><Alt>d" # keep this shortcut note

[audio]
sample_rate = 48000 # edited while the daemon was running
channels = 1

[future_plugin]
enabled = true
""",
        encoding="utf-8",
    )

    await proxy.call_set_shortcut("<Control><Alt>d")

    saved = config_path.read_text(encoding="utf-8")
    assert "# Keep this user documentation." in saved
    assert 'future_root_setting = "untouched"' in saved
    assert 'trigger = "<Control><Alt>d" # keep this shortcut note' in saved
    assert "sample_rate = 48000 # edited while the daemon was running" in saved
    assert "[future_plugin]" in saved
    assert "enabled = true" in saved
