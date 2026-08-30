# ABOUTME: Exercises the model download D-Bus surface against the running daemon.
# ABOUTME: Uses an unknown model name so no test ever reaches the model host.

import os
from pathlib import Path

import pytest
from dbus_next.errors import DBusError

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"

# Not one of the models the daemon knows how to fetch, so the request fails
# before any network access. Keeps the suite offline and fast.
UNKNOWN_MODEL = "voxkey-integration-test-model"
KNOWN_MODEL = "parakeet-tdt-0.6b-v3"


async def _daemon_interface(dbus_session):
    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)


@pytest.mark.asyncio
async def test_repeated_download_requests_keep_the_daemon_healthy(
    daemon_process, dbus_session,
):
    """Pressing Download more than once must stay safe.

    Invalid requests must fail synchronously without destabilizing the daemon.
    Joining repeated valid requests is covered without network access by the
    unit test in src/dbus.rs.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_interface(dbus_session)

    for _ in range(3):
        with pytest.raises(DBusError):
            await daemon.call_download_model(UNKNOWN_MODEL)

    assert daemon_process.poll() is None, "daemon exited on repeated download requests"
    assert await daemon.call_model_status(UNKNOWN_MODEL) == "not_downloaded"
    # Still serving: a wedged daemon would not answer this.
    assert await daemon.get_state() == "Idle"


@pytest.mark.asyncio
async def test_status_of_a_model_that_was_never_downloaded(
    daemon_process, dbus_session,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_interface(dbus_session)

    assert await daemon.call_model_status(UNKNOWN_MODEL) == "not_downloaded"


@pytest.mark.asyncio
async def test_cancel_requires_an_active_catalog_download(
    daemon_process, dbus_session,
):
    """Cancellation is explicit, validated, and harmless when nothing runs."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_interface(dbus_session)

    for model_name in (UNKNOWN_MODEL, KNOWN_MODEL):
        with pytest.raises(DBusError):
            await daemon.call_cancel_model_download(model_name)

    assert daemon_process.poll() is None, "daemon exited on an idle cancel request"
    assert await daemon.get_state() == "Idle"


@pytest.mark.asyncio
async def test_a_failed_download_leaves_nothing_on_disk(
    daemon_process, dbus_session,
):
    """A rejected model name must not create a download directory."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_interface(dbus_session)

    data_home = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    model_dir = Path(data_home) / "voxkey" / "models" / UNKNOWN_MODEL

    with pytest.raises(DBusError):
        await daemon.call_download_model(UNKNOWN_MODEL)

    assert not model_dir.exists(), (
        f"a failed download left {model_dir} behind"
    )
    assert await daemon.call_model_status(UNKNOWN_MODEL) == "not_downloaded"


@pytest.mark.asyncio
async def test_deleting_a_model_cannot_target_anything_outside_the_catalogue(
    daemon_process, dbus_session,
):
    """The D-Bus method must reject paths and arbitrary leaf names."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_interface(dbus_session)
    data_home = Path(os.environ["XDG_DATA_HOME"])
    sentinel = data_home / "voxkey" / "must-not-delete"
    sentinel.write_text("important")

    for invalid_name in ("", "../must-not-delete", str(sentinel), UNKNOWN_MODEL):
        with pytest.raises(DBusError):
            await daemon.call_delete_model(invalid_name)

    assert sentinel.read_text() == "important"
