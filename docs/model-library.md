# Model library

Voxkey's model library is a curated set of downloadable speech-to-text models
that the packaged daemon can run itself. The catalog deliberately includes
only models with a native runtime path, stable downloadable artifacts, clear
license terms, and practical CPU operation. Research was last reviewed on
2026-08-29.

## Included models

| Model | Released | Language coverage | Voxkey runtime | Download | License |
| --- | --- | --- | --- | ---: | --- |
| [Nemotron 3.5 ASR Streaming 0.6B](https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b) | 2026-06-04 | 35 languages across 40 locales | Live, cache-aware streaming | 682 MB | [OpenMDW 1.1](https://openmdw.ai/license/1-1/) |
| [Parakeet Unified English 0.6B](https://huggingface.co/nvidia/parakeet-unified-en-0.6b) | 2026-04-07 | English | Live, buffered streaming | 663 MB | [NVIDIA Open Model License](https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/) |
| [Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) | 2025-05 | 25 European languages | After recording | 670 MB | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |
| [Parakeet TDT 0.6B v2](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2) | 2025-04 | English | After recording | 661 MB | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |

The first two models publish revisable text while the microphone is active.
The v2 and v3 models decode the completed recording, with Voxkey's normal live
preview option available before the final pass.

Every model view in Settings offers **Cancel download** while a transfer is in
progress. Cancelling removes the incomplete file currently being received but
keeps already verified files, so starting the download again does not repeat
finished work.

“Downloadable” does not mean every model uses an OSI-approved open-source
license. Parakeet v2/v3 use Creative Commons; Nemotron 3.5 and Parakeet Unified
use publisher-specific open-weight licenses. The settings app always shows the
applicable license beside the model card so users can evaluate its terms before
downloading.

Voxkey runs int8 ONNX conversions maintained by the sherpa-onnx project. The
[Nemotron streaming documentation](https://k2-fsa.github.io/sherpa/onnx/nemo/nemotron-streaming.html)
and the pinned [Parakeet Unified conversion](https://huggingface.co/csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-560ms/tree/7551fd26fc810cc1e4e043e608db4d13b59be31e)
describe those runtime artifacts.

## Other strong models evaluated

These models are useful choices, but are not one-click installs in the current
library:

| Model | Why it is interesting | Why it is deferred |
| --- | --- | --- |
| [Qwen3-ASR 0.6B](https://huggingface.co/Qwen/Qwen3-ASR-0.6B) | Apache 2.0; 30 languages plus 22 Chinese dialects; offline and streaming | Its generative decoder needs a different, larger runtime interface and the source checkpoint is about 1.9 GB. It should land as a separate runtime module rather than complicating the transducer path. |
| [Whisper large-v3-turbo](https://huggingface.co/openai/whisper-large-v3-turbo) | MIT; 99 languages; broad ecosystem support | Voxkey currently treats whisper.cpp as a user-supplied executable. Downloading only the model would not create a dependable one-click setup until Voxkey also owns or packages that runtime. |
| [Distil-Whisper large-v3.5](https://huggingface.co/distil-whisper/distil-large-v3.5) | MIT; efficient and accurate English Whisper variant | It has the same runtime-ownership issue as Whisper Turbo and is English-only. It remains usable through Voxkey's advanced whisper.cpp configuration. |
| [Meta Omnilingual ASR 300M](https://huggingface.co/facebook/omniASR-LLM-300M) | Apache 2.0; coverage across more than 1,600 languages | The LLM checkpoint is roughly 6.5 GB and does not fit the current lightweight native runtime or the intended download/RAM envelope. |

This is a compatibility decision, not a quality ranking. A model becomes a
library candidate when Voxkey can install it safely and run it end to end from
the RPM without asking the user to assemble a second toolchain.

## Transcription server contract

Every catalog model can instead be selected with **Run model → On a server**.
Voxkey sends an OpenAI-compatible multipart `POST` to the configured address:

- `file`: the recorded WAV
- `model`: the selected catalog ID (or a preserved custom ID)
- `prompt`: optional important vocabulary
- `Authorization: Bearer …`: optional, when a server API key is saved

The server returns JSON containing a `text` field. Voxkey stores the optional
key in the desktop keyring, checks the route without sending audio or the key,
requires HTTPS for public servers, and allows plain HTTP only for loopback or
an explicitly approved literal private-network address. Long recordings are
split into bounded overlapping requests and merged at word boundaries.

## Catalog maintenance rules

Adding a model requires all of the following:

1. A model card from the publisher with language, release, and license facts.
2. A sherpa-onnx artifact set that the RPM's native runtime can execute.
3. An immutable source revision plus exact file sizes and SHA-256 hashes.
4. Catalog/manifest consistency tests and local decode tests.
5. Download, selection, transcription, history, and text-insertion validation
   in the Fedora GNOME VM.

Downloads land in the user's Voxkey data directory only after every artifact
matches its pinned identity. Interrupted, oversized, symlinked, or mismatched
files are rejected rather than treated as installed models.
