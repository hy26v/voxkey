#!/usr/bin/env python3
# ABOUTME: Drives real dictation and grades preview stability plus final accuracy.
# ABOUTME: Compares every run with a committed per-strategy quality baseline.
"""Preview quality harness for a real Wayland/PipeWire Voxkey session.

The harness plays fixture WAVs through a virtual microphone, captures every
LiveTranscript change, grades preview churn/fidelity/stabilization lag, and
grades the final transcript against committed fixture ground truth.

Example:
    python3 scripts/preview_quality.py \
        --files tests/fixtures/hello_world.wav tests/fixtures/long_passage.wav \
        --strategy whole --engine-id whisper.cpp/ggml-base.bin@sha256:... \
        --output /tmp/preview_quality.json
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
import unicodedata
from difflib import SequenceMatcher
from pathlib import Path

BUS = "io.github.hy26v.Voxkey.Daemon"
OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
IFACE = "io.github.hy26v.Voxkey.Daemon1"
SINK_PREFIX = "voxkey_quality_mic"
ROOT = Path(__file__).resolve().parents[1]
DEFAULT_GROUND_TRUTH = ROOT / "tests/fixtures/transcripts.json"
BASELINE_DIR = ROOT / "scripts/preview_baselines"
DEFAULT_FILES = [
    ROOT / "tests/fixtures/long_passage.wav",
    ROOT / "tests/fixtures/punctuation.wav",
    ROOT / "tests/fixtures/the_quick_brown_fox.wav",
]
NUMBER_WORDS = {
    "zero": "0",
    "one": "1",
    "two": "2",
    "three": "3",
    "four": "4",
    "five": "5",
    "six": "6",
    "seven": "7",
    "eight": "8",
    "nine": "9",
    "ten": "10",
}


def busctl(*args):
    result = subprocess.run(
        ["busctl", "--user", *args], capture_output=True, text=True, check=True
    )
    return result.stdout.strip()


def call(method):
    busctl("call", BUS, OBJECT_PATH, IFACE, method)


def call_string(method, value):
    busctl("call", BUS, OBJECT_PATH, IFACE, method, "s", value)


def get_property(name):
    # Human-readable busctl strings use systemd/C escapes rather than JSON
    # escapes. Structured output is lossless for arbitrary transcript text.
    response = json.loads(
        busctl("--json=short", "get-property", BUS, OBJECT_PATH, IFACE, name)
    )
    if "data" not in response:
        raise ValueError(f"busctl returned no data for {name}: {response!r}")
    return response["data"]


def normalize(text):
    folded = unicodedata.normalize("NFKC", text).casefold()
    folded = "".join(
        " " if unicodedata.category(character)[0] in {"P", "S"} else character
        for character in folded
    )
    return [NUMBER_WORDS.get(word, word) for word in folded.split() if word]


def word_f1(reference, hypothesis):
    if not reference and not hypothesis:
        return 1.0
    if not reference or not hypothesis:
        return 0.0
    ref_counts = {}
    for word in reference:
        ref_counts[word] = ref_counts.get(word, 0) + 1
    hits = 0
    for word in hypothesis:
        if ref_counts.get(word, 0) > 0:
            hits += 1
            ref_counts[word] -= 1
    precision = hits / len(hypothesis)
    recall = hits / len(reference)
    return 0.0 if precision + recall == 0 else 2 * precision * recall / (precision + recall)


def dropped_words(old_words, new_words):
    """Count words removed between adjacent previews, retaining duplicates."""
    counts = {}
    for word in new_words:
        counts[word] = counts.get(word, 0) + 1
    dropped = 0
    for word in old_words:
        if counts.get(word, 0) > 0:
            counts[word] -= 1
        else:
            dropped += 1
    return dropped


def percentile(values, percent):
    if not values:
        return None
    ordered = sorted(values)
    index = round((len(ordered) - 1) * percent / 100)
    return ordered[index]


def stable_word_lags(timeline, reference_words):
    """Seconds from a correct word's first appearance until it never changes.

    Matching is positional and prefix-based: a word is stable only when it and
    every word before it equal the final reference in all subsequent updates.
    """
    updates = [
        (entry["t"], normalize(entry["text"]))
        for entry in timeline
        if entry.get("text")
    ]
    lags = []
    for position in range(len(reference_words)):
        expected = reference_words[: position + 1]
        matching = [
            index
            for index, (_, words) in enumerate(updates)
            if words[: position + 1] == expected
        ]
        if not matching:
            continue
        first = matching[0]
        stable = next(
            (
                index
                for index in matching
                if all(
                    later_words[: position + 1] == expected
                    for _, later_words in updates[index:]
                )
            ),
            None,
        )
        if stable is not None:
            lags.append(max(0.0, updates[stable][0] - updates[first][0]))
    return lags


def sequence_ratio(reference, hypothesis):
    return SequenceMatcher(None, " ".join(reference), " ".join(hypothesis)).ratio()


def load_ground_truth(path, files):
    with open(path, encoding="utf-8") as handle:
        fixture_text = json.load(handle)
    missing = [Path(file).name for file in files if Path(file).name not in fixture_text]
    if missing:
        raise ValueError(f"ground truth is missing fixtures: {', '.join(missing)}")
    return " ".join(fixture_text[Path(file).name].strip() for file in files).strip()


def evaluate(timeline, last_preview, final, ground_truth):
    final_words = normalize(final)
    preview_words = normalize(last_preview)
    truth_words = normalize(ground_truth)
    churn = 0
    updates = 0
    previous_words = []
    for entry in timeline:
        if not entry["text"]:
            continue
        updates += 1
        words = normalize(entry["text"])
        churn += dropped_words(previous_words, words)
        previous_words = words

    lags = stable_word_lags(timeline, final_words)
    first_preview = next(
        (entry["t"] for entry in timeline if entry.get("text")), None
    )
    preview_f1 = word_f1(final_words, preview_words)
    preview_ratio = sequence_ratio(final_words, preview_words)
    truth_f1 = word_f1(truth_words, final_words)
    truth_ratio = sequence_ratio(truth_words, final_words)
    return {
        "updates": updates,
        "dropped_words_total": churn,
        "time_to_first_preview_seconds": first_preview,
        "stable_word_count": len(lags),
        "stable_word_lag_mean_seconds": round(sum(lags) / len(lags), 3) if lags else None,
        "stable_word_lag_p95_seconds": round(percentile(lags, 95), 3) if lags else None,
        "stable_word_lag_max_seconds": round(max(lags), 3) if lags else None,
        "preview_final_word_f1": round(preview_f1, 3),
        "preview_final_sequence_ratio": round(preview_ratio, 3),
        # Compatibility aliases for older result consumers.
        "final_word_f1": round(preview_f1, 3),
        "final_sequence_ratio": round(preview_ratio, 3),
        "final_ground_truth_word_f1": round(truth_f1, 3),
        "final_ground_truth_sequence_ratio": round(truth_ratio, 3),
        "hallucinated_preview_words": sorted(set(preview_words) - set(final_words)),
        "final_words_missing_from_preview": sorted(set(final_words) - set(preview_words)),
        "hallucinated_final_words": sorted(set(final_words) - set(truth_words)),
        "ground_truth_words_missing_from_final": sorted(set(truth_words) - set(final_words)),
        "last_preview": last_preview,
        "final_transcript": final,
        "ground_truth": ground_truth,
    }


def compare_baseline(metrics, baseline):
    observed = baseline.get("metrics", {})
    policies = {
        # The two retained hypothesis sentences may correct a word; larger
        # churn indicates that confirmed/context text regressed.
        "dropped_words_total": ("max", 2.0),
        # One polling boundary plus modest decode jitter is expected between
        # otherwise identical real-model runs.
        "time_to_first_preview_seconds": ("max", 1.25),
        "stable_word_lag_p95_seconds": ("max", 0.25),
        "preview_final_word_f1": ("min", 0.02),
        "final_ground_truth_word_f1": ("min", 0.02),
    }
    regressions = []
    for name, (direction, tolerance) in policies.items():
        previous = observed.get(name)
        current = metrics.get(name)
        if previous is None or current is None:
            continue
        if direction == "max" and current > previous + tolerance:
            regressions.append(
                f"{name} regressed from {previous} to {current} (allowed +{tolerance})"
            )
        elif direction == "min" and current < previous - tolerance:
            regressions.append(
                f"{name} regressed from {previous} to {current} (allowed -{tolerance})"
            )
    return regressions


def pactl(*args, check=True):
    return subprocess.run(
        ["pactl", *args], capture_output=True, text=True, check=check
    ).stdout.strip()


def setup_virtual_mic():
    previous_source = pactl("get-default-source")
    sink = f"{SINK_PREFIX}_{os.getpid()}"
    module_id = pactl("load-module", "module-null-sink", f"sink_name={sink}")
    try:
        pactl("set-default-source", f"{sink}.monitor")
    except Exception:
        pactl("unload-module", module_id, check=False)
        raise
    return module_id, previous_source, sink


def teardown_virtual_mic(module_id, previous_source):
    if previous_source:
        pactl("set-default-source", previous_source, check=False)
    if module_id:
        pactl("unload-module", module_id, check=False)


def wait_for_session_ready(timeout=20):
    deadline = time.monotonic() + timeout
    stable = 0
    while time.monotonic() < deadline:
        ready = get_property("State") == "Idle" and get_property("PortalConnected") is True
        stable = stable + 1 if ready else 0
        if stable >= 2:
            return
        time.sleep(0.25)
    raise RuntimeError("daemon did not return to a connected Idle session")


def configure_preview(strategy, interval_ms):
    original = json.loads(get_property("PreviewConfig"))
    selected = dict(original)
    selected.update(
        {
            "mode": "always",
            "strategy": strategy,
            "interval_ms": interval_ms,
            "max_audio_seconds": 0,
        }
    )
    if selected != original:
        call_string("SetPreviewConfig", json.dumps(selected, separators=(",", ":")))
        # The setter rebuilds portal/audio session state asynchronously.
        time.sleep(0.5)
        wait_for_session_ready()
    return original, selected != original


def restore_preview_config(original, changed):
    if not changed:
        return
    call_string("SetPreviewConfig", json.dumps(original, separators=(",", ":")))
    time.sleep(0.5)
    wait_for_session_ready()


def run_session(files, pause, settle):
    if get_property("State") != "Idle" or get_property("PortalConnected") is not True:
        raise RuntimeError("daemon is not connected and Idle; finish any active dictation first")
    module_id, previous_source, sink = setup_virtual_mic()
    timeline = []
    try:
        call("StartDictation")
        deadline = time.monotonic() + 10
        while get_property("State") != "Recording":
            if time.monotonic() > deadline:
                raise RuntimeError("daemon did not start recording")
            time.sleep(0.2)
        started = time.monotonic()
        last = None

        def sample(stage):
            nonlocal last
            live = get_property("LiveTranscript")
            if live != last:
                timeline.append(
                    {
                        "t": round(time.monotonic() - started, 3),
                        "stage": stage,
                        "text": live,
                    }
                )
                last = live

        for index, path in enumerate(files):
            if index:
                end = time.monotonic() + pause
                while time.monotonic() < end:
                    sample("pause")
                    time.sleep(0.2)
            player = subprocess.Popen(
                ["pw-cat", "--playback", "--target", sink, path],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            while player.poll() is None:
                sample("speech")
                time.sleep(0.2)
            sample("speech")

        end = time.monotonic() + settle
        while time.monotonic() < end:
            sample("settle")
            time.sleep(0.2)

        stop = subprocess.Popen(
            ["busctl", "--user", "call", BUS, OBJECT_PATH, IFACE, "StopDictation"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + 180
        while stop.poll() is None or get_property("State") != "Idle":
            sample("finalizing")
            if time.monotonic() > deadline:
                stop.kill()
                raise RuntimeError("daemon did not finish transcribing")
            time.sleep(0.2)
        stdout, stderr = stop.communicate()
        if stop.returncode:
            raise RuntimeError(f"StopDictation failed: {(stderr or stdout).strip()}")
        last_preview = next(
            (entry["text"] for entry in reversed(timeline) if entry.get("text")),
            "",
        )
        final = get_property("LastTranscript")
        return timeline, last_preview, final
    finally:
        try:
            if get_property("State") != "Idle":
                call("CancelDictation")
        except Exception:
            pass
        teardown_virtual_mic(module_id, previous_source)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--files",
        nargs="+",
        default=[str(path) for path in DEFAULT_FILES],
        help="fixture WAVs; defaults to the committed multi-sentence decision set",
    )
    parser.add_argument("--strategy", choices=("whole", "segmented"), default="whole")
    parser.add_argument("--pause", type=float, default=2.0)
    parser.add_argument("--settle", type=float, default=3.0)
    parser.add_argument("--interval-ms", type=int, default=1000)
    parser.add_argument(
        "--engine-id",
        default="unspecified",
        help="stable backend/model fingerprint used to select a comparable baseline",
    )
    parser.add_argument("--output", default="/tmp/preview_quality.json")
    parser.add_argument("--ground-truth", default=str(DEFAULT_GROUND_TRUTH))
    parser.add_argument("--baseline", help="baseline JSON; defaults to the selected strategy")
    parser.add_argument("--skip-baseline", action="store_true")
    parser.add_argument("--update-baseline", action="store_true")
    args = parser.parse_args()
    if args.interval_ms < 250:
        parser.error("--interval-ms must be at least 250")
    args.engine_id = args.engine_id.strip()
    if not args.engine_id:
        parser.error("--engine-id cannot be blank")
    files = [str(Path(path).resolve()) for path in args.files]
    baseline_path = Path(args.baseline) if args.baseline else BASELINE_DIR / f"{args.strategy}.json"
    fixture_names = [Path(path).name for path in files]
    run_settings = {
        "engine_id": args.engine_id,
        "fixtures": fixture_names,
        "pause_seconds": args.pause,
        "settle_seconds": args.settle,
        "interval_ms": args.interval_ms,
    }
    baseline = None
    if not args.update_baseline and not args.skip_baseline:
        with open(baseline_path, encoding="utf-8") as handle:
            baseline = json.load(handle)
        if baseline.get("strategy") != args.strategy:
            raise ValueError(
                f"baseline strategy {baseline.get('strategy')!r} does not match {args.strategy!r}"
            )
        for setting, current in run_settings.items():
            if baseline.get(setting) != current:
                raise ValueError(
                    f"baseline {setting} {baseline.get(setting)!r} does not match {current!r}; "
                    "select a matching --baseline or use --update-baseline"
                )

    ground_truth = load_ground_truth(args.ground_truth, files)
    original_preview = None
    preview_changed = False
    try:
        original_preview, preview_changed = configure_preview(args.strategy, args.interval_ms)
        timeline, last_preview, final = run_session(files, args.pause, args.settle)
    finally:
        if original_preview is not None:
            restore_preview_config(original_preview, preview_changed)
    metrics = evaluate(timeline, last_preview, final, ground_truth)
    regressions = []
    if args.update_baseline:
        baseline_path.parent.mkdir(parents=True, exist_ok=True)
        with open(baseline_path, "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "schema_version": 2,
                    "strategy": args.strategy,
                    **run_settings,
                    "metrics": metrics,
                },
                handle,
                indent=2,
                ensure_ascii=False,
            )
            handle.write("\n")
    elif not args.skip_baseline:
        assert baseline is not None
        regressions = compare_baseline(metrics, baseline)

    result = {
        "strategy": args.strategy,
        "files": files,
        "settings": run_settings,
        "timeline": timeline,
        "metrics": metrics,
        "baseline": str(baseline_path),
        "regressions": regressions,
    }
    with open(args.output, "w", encoding="utf-8") as handle:
        json.dump(result, handle, indent=2, ensure_ascii=False)
        handle.write("\n")

    print(f"preview updates:          {metrics['updates']}")
    print(f"words dropped in churn:   {metrics['dropped_words_total']}")
    print(f"stable-word p95 lag:      {metrics['stable_word_lag_p95_seconds']}")
    print(f"preview F1 vs final:      {metrics['preview_final_word_f1']}")
    print(f"final F1 vs ground truth: {metrics['final_ground_truth_word_f1']}")
    print(f"last preview:             {re.sub(r'\s+', ' ', last_preview)}")
    print(f"final transcript:         {re.sub(r'\s+', ' ', final)}")
    print(f"full timeline:            {args.output}")
    if regressions:
        print("quality regressions:", file=sys.stderr)
        for regression in regressions:
            print(f"  - {regression}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
