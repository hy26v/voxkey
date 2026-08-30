# ABOUTME: Verifies integration tests isolate mutable Voxkey state and audio routing.
# ABOUTME: Prevents pytest from changing live config, portal tokens, history, or microphones.

import os
from pathlib import Path

import pytest

from helpers.virtual_microphone import _require_isolated_audio_runtime


def test_voxkey_paths_are_isolated(isolated_voxkey_home):
    config_home = Path(os.environ["XDG_CONFIG_HOME"]).resolve()
    data_home = Path(os.environ["XDG_DATA_HOME"]).resolve()
    state_home = Path(os.environ["XDG_STATE_HOME"]).resolve()
    token_path = Path(os.environ["VOXKEY_RESTORE_TOKEN_PATH"]).resolve()

    assert config_home == isolated_voxkey_home.resolve()
    assert token_path == config_home / "voxkey" / "restore_token"
    assert data_home == config_home / "data"
    assert (data_home / "voxkey").is_dir()
    assert (data_home / "voxkey" / "models").is_dir()
    assert not (data_home / "voxkey" / "models").is_symlink()
    assert state_home.is_dir()
    assert state_home != (Path.home() / ".local" / "state").resolve()

    source = os.environ.get("VOXKEY_TEST_CONFIG")
    if source:
        assert (config_home / "voxkey" / "config.toml").read_bytes() == \
            Path(source).read_bytes()


def test_virtual_microphone_rejects_an_unmarked_runtime(monkeypatch, tmp_path):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.delenv("VOXKEY_TEST_AUDIO_ISOLATED", raising=False)

    with pytest.raises(RuntimeError, match="live default microphone"):
        _require_isolated_audio_runtime()


def test_virtual_microphone_rejects_the_live_user_runtime(monkeypatch, tmp_path):
    live_runtime = tmp_path / "run" / "user" / "1000"
    live_runtime.mkdir(parents=True)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(live_runtime))
    monkeypatch.setenv("VOXKEY_TEST_AUDIO_ISOLATED", str(live_runtime))

    with pytest.raises(RuntimeError, match="isolated Voxkey PipeWire session"):
        _require_isolated_audio_runtime()


def test_virtual_microphone_accepts_the_launcher_runtime(monkeypatch, tmp_path):
    isolated = tmp_path / "voxkey-ci-audio.test"
    isolated.mkdir()
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(isolated))
    monkeypatch.setenv("VOXKEY_TEST_AUDIO_ISOLATED", str(isolated))

    _require_isolated_audio_runtime()
