# ABOUTME: Verifies startup secures a permissive config before keyring migration can fail.
# ABOUTME: Uses the private bus where Secret Service is deliberately unavailable.

import os
import stat

import pytest


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    path = isolated_voxkey_home / "voxkey" / "config.toml"
    path.write_text(
        '''[transcriber.mistral]
api_key = "integration-plaintext-secret"
'''
    )
    os.chmod(path, 0o644)
    yield


def test_startup_secures_plaintext_secret_when_keyring_is_unavailable(
    daemon_process,
    isolated_voxkey_home,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    path = isolated_voxkey_home / "voxkey" / "config.toml"

    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert "integration-plaintext-secret" in path.read_text(), (
        "the isolated bus unexpectedly provided a keyring, so this did not "
        "exercise the migration-failure branch"
    )
