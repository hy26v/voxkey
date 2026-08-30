# ABOUTME: Runs the preview decision harness through a real whisper.cpp binary on isolated audio.
# ABOUTME: Verifies default decode flags, final accuracy, stable previews, and committed baselines.

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
DECISION_FIXTURES = (
    "long_passage.wav",
    "punctuation.wav",
    "the_quick_brown_fox.wav",
)


def _whisper_engine_id(model):
    digest = hashlib.sha256()
    with open(model, "rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return f"whisper.cpp/{Path(model).name}@sha256:{digest.hexdigest()}"


@pytest.fixture
def daemon_config(isolated_voxkey_home, tmp_path, monkeypatch):
    real_binary = os.environ.get("VOXKEY_TEST_WHISPER_BIN")
    model = os.environ.get("VOXKEY_TEST_WHISPER_MODEL")
    if not real_binary or not model:
        pytest.skip(
            "set VOXKEY_TEST_WHISPER_BIN and VOXKEY_TEST_WHISPER_MODEL for the real-model run"
        )
    if not Path(real_binary).is_file() or not Path(model).is_file():
        pytest.skip("the configured whisper.cpp binary or model does not exist")

    argv_log = tmp_path / "whisper-argv.jsonl"
    wrapper = tmp_path / "whisper-cli"
    wrapper.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, sys\n"
        "with open(os.environ['VOXKEY_WHISPER_ARGV_LOG'], 'a', encoding='utf-8') as log:\n"
        "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
        "real = os.environ['VOXKEY_REAL_WHISPER_BIN']\n"
        "os.execv(real, [real, *sys.argv[1:]])\n"
    )
    wrapper.chmod(0o755)
    monkeypatch.setenv("VOXKEY_WHISPER_ARGV_LOG", str(argv_log))
    monkeypatch.setenv("VOXKEY_REAL_WHISPER_BIN", str(Path(real_binary).resolve()))

    config = isolated_voxkey_home / "voxkey/config.toml"
    config.write_text(
        "\n".join(
            [
                "[transcriber]",
                'provider = "whisper-cpp"',
                "",
                "[transcriber.whisper_cpp]",
                f"command = {json.dumps(str(wrapper))}",
                "args = [",
                f"  \"-m\", {json.dumps(str(Path(model).resolve()))},",
                '  "-l", "en", "{audio_file}",',
                "]",
                "",
                "[preview]",
                'mode = "always"',
                'strategy = "whole"',
                "interval_ms = 1000",
                "max_audio_seconds = 0",
            ]
        )
        + "\n"
    )
    yield argv_log, _whisper_engine_id(Path(model).resolve())


@pytest.mark.parametrize("strategy", ["whole", "segmented"])
def test_real_whisper_preview_quality_baseline(
    strategy,
    daemon_process,
    daemon_config,
    tmp_path,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    update_committed = os.environ.get("VOXKEY_UPDATE_PREVIEW_BASELINES") == "1"
    argv_log, engine_id = daemon_config
    baseline = ROOT / f"scripts/preview_baselines/{strategy}.json"
    output = tmp_path / f"{strategy}-result.json"
    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = daemon_process.bus_address

    arguments = [
        sys.executable,
        str(ROOT / "scripts/preview_quality.py"),
        "--strategy",
        strategy,
        "--baseline",
        str(baseline),
        "--engine-id",
        engine_id,
        "--output",
        str(output),
    ]
    if update_committed:
        arguments.append("--update-baseline")
    result = subprocess.run(
        arguments,
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert result.returncode == 0, f"{result.stdout}\n{result.stderr}"
    report = json.loads(output.read_text())
    metrics = report["metrics"]
    assert metrics["updates"] >= 1
    if strategy == "whole":
        # Confirmed text is immutable; permit only tiny corrections inside the
        # two sentence hypotheses that the agreement policy retains.
        assert metrics["dropped_words_total"] <= 2
    else:
        # Segmented mode may revise its one open utterance; committed segments
        # are still immutable, and the baseline catches any increase.
        assert metrics["dropped_words_total"] <= 4
    assert metrics["final_ground_truth_word_f1"] >= 0.8
    assert metrics["preview_final_word_f1"] >= 0.8
    assert daemon_process.poll() is None, "real-model injection killed the isolated daemon"

    invocations = [
        json.loads(line) for line in argv_log.read_text().splitlines() if line
    ]
    assert invocations
    for arguments in invocations:
        assert "--no-timestamps" in arguments
        assert "--flash-attn" in arguments
        assert "--no-fallback" in arguments
        assert "--suppress-nst" in arguments
        assert "--vad" in arguments
        assert "--vad-model" in arguments
        prompt_index = arguments.index("--prompt")
        assert arguments[prompt_index + 1].startswith(
            "Hello, how are you doing? Nice to meet you."
        )


def test_real_whisper_snr_sweep_keeps_segmented_previews_live(
    daemon_process,
    daemon_config,
    tmp_path,
):
    del daemon_config  # The fixture selects and logs the real backend.
    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg is required for the SNR sweep")
    assert daemon_process.reached_idle, daemon_process.startup_lines

    clean = tmp_path / "clean"
    degraded = tmp_path / "degraded"
    clean.mkdir()
    for name in DECISION_FIXTURES:
        shutil.copyfile(ROOT / "tests/fixtures" / name, clean / name)
    subprocess.run(
        [
            str(ROOT / "scripts/degrade_fixtures.sh"),
            str(clean),
            str(degraded),
            "--snr-sweep",
            "20,10,5,0",
        ],
        check=True,
        capture_output=True,
        text=True,
    )

    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = daemon_process.bus_address
    observed = {}
    for level in (20, 10, 5, 0):
        output = tmp_path / f"snr-{level}.json"
        files = [str(degraded / f"snr-{level}db" / name) for name in DECISION_FIXTURES]
        result = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts/preview_quality.py"),
                "--strategy",
                "segmented",
                "--files",
                *files,
                "--output",
                str(output),
                "--skip-baseline",
            ],
            cwd=ROOT,
            env=env,
            capture_output=True,
            text=True,
            timeout=300,
        )
        assert result.returncode == 0, f"SNR {level} dB:\n{result.stdout}\n{result.stderr}"
        metrics = json.loads(output.read_text())["metrics"]
        observed[level] = metrics
        assert metrics["updates"] >= 1, f"no previews at {level} dB"
        assert metrics["preview_final_word_f1"] >= 0.75, metrics

    # The sweep is principally a VAD/preview-liveness boundary test; still
    # require useful final recognition through the deliberately harsh 5 dB
    # case so a silent or hallucinated final cannot make the preview look good.
    assert observed[5]["final_ground_truth_word_f1"] >= 0.7
