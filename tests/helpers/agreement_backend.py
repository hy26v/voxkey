#!/usr/bin/env python3
# ABOUTME: Deterministic WAV-aware backend for agreement/lookback integration tests.
# ABOUTME: Logs every decode window and returns five stable punctuated sentences.

import json
import os
import sys
import time
import wave


with wave.open(sys.argv[1], "rb") as audio:
    frames = audio.getnframes()
    rate = audio.getframerate()
    audio.setpos(max(0, frames - rate))
    tail = audio.readframes(min(frames, rate))
    observation = {
        "at": time.monotonic(),
        "frames": frames,
        "rate": rate,
        "one_second_silent_tail": len(tail) > 0 and not any(tail),
    }

with open(os.environ["VOXKEY_AGREEMENT_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(observation) + "\n")

print(
    "one two three four five. "
    "six seven eight nine ten. "
    "eleven twelve thirteen fourteen fifteen. "
    "sixteen seventeen eighteen nineteen twenty. "
    "twenty-one twenty-two twenty-three twenty-four twenty-five."
)
