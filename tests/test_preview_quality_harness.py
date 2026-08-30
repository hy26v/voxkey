# ABOUTME: Keeps the preview decision harness, fixture truth, and baselines executable in CI.
# ABOUTME: Exercises normalization, stability lag, regression policy, and the SNR degrader.

import importlib.util
import json
import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "preview_quality", ROOT / "scripts/preview_quality.py"
)
PREVIEW_QUALITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREVIEW_QUALITY)


def test_ground_truth_covers_every_committed_wav_fixture():
    truth = json.loads((ROOT / "tests/fixtures/transcripts.json").read_text())
    fixtures = {path.name for path in (ROOT / "tests/fixtures").glob("*.wav")}

    assert set(truth) == fixtures
    assert all(text.strip() for text in truth.values())


def test_normalization_handles_unicode_punctuation_and_number_formatting():
    assert PREVIEW_QUALITY.normalize("Über—señor: one, TWO, 3!") == [
        "über",
        "señor",
        "1",
        "2",
        "3",
    ]


def test_stable_word_lag_waits_until_a_rewritten_word_stops_changing():
    timeline = [
        {"t": 0.0, "text": "one two"},
        {"t": 1.0, "text": "one wrong"},
        {"t": 2.0, "text": "one two"},
        {"t": 3.0, "text": "one two"},
    ]

    assert PREVIEW_QUALITY.stable_word_lags(
        timeline, PREVIEW_QUALITY.normalize("one two")
    ) == [0.0, 2.0]


def test_baseline_policy_reports_only_material_regressions():
    baseline = {
        "metrics": {
            "dropped_words_total": 0,
            "time_to_first_preview_seconds": 1.0,
            "stable_word_lag_p95_seconds": 0.5,
            "preview_final_word_f1": 0.9,
            "final_ground_truth_word_f1": 0.95,
        }
    }
    within_tolerance = {
        "dropped_words_total": 0,
        "time_to_first_preview_seconds": 1.5,
        "stable_word_lag_p95_seconds": 0.75,
        "preview_final_word_f1": 0.88,
        "final_ground_truth_word_f1": 0.93,
    }
    regressed = dict(within_tolerance, dropped_words_total=3)

    assert PREVIEW_QUALITY.compare_baseline(within_tolerance, baseline) == []
    assert "dropped_words_total" in " ".join(
        PREVIEW_QUALITY.compare_baseline(regressed, baseline)
    )


@pytest.mark.parametrize("strategy", ["whole", "segmented"])
def test_committed_baseline_matches_the_default_decision_set(strategy):
    path = ROOT / f"scripts/preview_baselines/{strategy}.json"
    baseline = json.loads(path.read_text())

    assert baseline["schema_version"] == 2
    assert baseline["strategy"] == strategy
    assert baseline["engine_id"].startswith("whisper.cpp/ggml-base.bin@sha256:")
    assert baseline["fixtures"] == [path.name for path in PREVIEW_QUALITY.DEFAULT_FILES]
    assert baseline["interval_ms"] >= 250
    assert baseline["metrics"]["final_ground_truth_word_f1"] >= 0


def test_snr_sweep_generates_each_level_and_a_manifest(tmp_path):
    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg is not installed")
    source = tmp_path / "source"
    output = tmp_path / "degraded"
    source.mkdir()
    shutil.copyfile(ROOT / "tests/fixtures/hello.wav", source / "hello.wav")

    subprocess.run(
        [
            str(ROOT / "scripts/degrade_fixtures.sh"),
            str(source),
            str(output),
            "--snr-sweep",
            "20,5",
        ],
        check=True,
        capture_output=True,
        text=True,
    )

    assert (output / "snr-20db/hello.wav").is_file()
    assert (output / "snr-5db/hello.wav").is_file()
    rows = (output / "snr-sweep.tsv").read_text().splitlines()
    assert len(rows) == 3
