#!/usr/bin/env bash
# ABOUTME: Adds band limiting and controlled pink noise to clean audio fixtures.
# ABOUTME: Supports a multi-level SNR sweep for finding VAD failure boundaries.
set -euo pipefail

usage='usage: degrade_fixtures.sh <source-dir> <dest-dir> [noise-amplitude]
       degrade_fixtures.sh <source-dir> <dest-dir> --snr-sweep [db-list]

db-list defaults to 30,20,15,10,5,0. Sweep outputs are placed in
<dest-dir>/snr-<N>db/ and summarized in <dest-dir>/snr-sweep.tsv.'

src="${1:-}"
dst="${2:-}"
mode="${3:-0.012}"
[[ -n "$src" && -n "$dst" ]] || { echo "$usage" >&2; exit 2; }

command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }
mkdir -p "$dst"

noise_for_snr() {
    python3 - "$1" "$2" <<'PY'
import math
import sys
import wave

path, snr_db = sys.argv[1], float(sys.argv[2])
with wave.open(path, "rb") as wav:
    if wav.getsampwidth() != 2:
        raise SystemExit(f"{path}: SNR sweep requires 16-bit PCM")
    raw = wav.readframes(wav.getnframes())
samples = [int.from_bytes(raw[i:i+2], "little", signed=True) / 32768.0 for i in range(0, len(raw), 2)]
rms = math.sqrt(sum(sample * sample for sample in samples) / max(1, len(samples)))
# anoisesrc uses peak amplitude; sqrt(3) converts the target RMS to the
# equivalent uniform-noise peak before its pink filter shapes the spectrum.
amplitude = min(0.95, rms * math.sqrt(3) / (10 ** (snr_db / 20)))
print(f"{amplitude:.8f}")
PY
}

degrade_one() {
    local input="$1"
    local output="$2"
    local noise="$3"
    ffmpeg -y -v error \
        -i "$input" \
        -f lavfi -i "anoisesrc=color=pink:sample_rate=16000:amplitude=${noise}" \
        -filter_complex "\
[0:a]aformat=sample_rates=16000:channel_layouts=mono,highpass=f=90,lowpass=f=7500,volume=0.95[c];\
[c][1:a]amix=inputs=2:duration=first:normalize=0,alimiter=limit=0.95[o]" \
        -map "[o]" -ar 16000 -ac 1 "$output"
}

if [[ "$mode" == "--snr-sweep" ]]; then
    snr_list="${4:-30,20,15,10,5,0}"
    manifest="$dst/snr-sweep.tsv"
    printf 'fixture\ttarget_snr_db\tnoise_amplitude\toutput\n' > "$manifest"
    IFS=',' read -r -a snrs <<< "$snr_list"
    for snr in "${snrs[@]}"; do
        [[ "$snr" =~ ^-?[0-9]+([.][0-9]+)?$ ]] || {
            echo "invalid SNR value: $snr" >&2
            exit 2
        }
        label="${snr//./p}"
        level_dir="$dst/snr-${label}db"
        mkdir -p "$level_dir"
        for input in "$src"/*.wav; do
            base="$(basename "$input")"
            noise="$(noise_for_snr "$input" "$snr")"
            output="$level_dir/$base"
            degrade_one "$input" "$output" "$noise"
            printf '%s\t%s\t%s\t%s\n' "$base" "$snr" "$noise" "$output" >> "$manifest"
            echo "degraded: $base (target SNR=${snr}dB, noise=${noise})"
        done
    done
    echo "SNR sweep manifest: $manifest"
else
    [[ "$mode" =~ ^0([.][0-9]+)?$|^1([.]0+)?$ ]] || {
        echo "noise amplitude must be between 0 and 1" >&2
        exit 2
    }
    for input in "$src"/*.wav; do
        base="$(basename "$input")"
        degrade_one "$input" "$dst/$base" "$mode"
        echo "degraded: $base (noise=${mode})"
    done
fi
