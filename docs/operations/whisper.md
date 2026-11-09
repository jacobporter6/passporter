---
description: Running whisper-server for dictation. Build it (CUDA or CPU), check it, tune the bridge's audio settings, and how long clips are kept.
sidebar:
  order: 2
sources:
  - deploy/whisper/build.sh
  - crates/bridge/src/audio/mod.rs
  - crates/bridge/src/config.rs
---

# Speech-to-text

[Dictation](../app/dictation.md) is transcribed on the dev machine by
whisper.cpp's `whisper-server`, built with CUDA when the machine has an
NVIDIA GPU, else for the CPU. The bridge is its only client and talks to it
over loopback: audio never leaves the machine. It is optional: without it,
the phone can still type.

| | |
| --- | --- |
| Service | `whisper-server.service` (systemd user unit), `127.0.0.1:8178` |
| Model | `large-v3-turbo` with CUDA, `base.en` on the CPU |
| Measured | An 11-second clip, as the phone sends it, in 0.44 s on a mid-range NVIDIA GPU (about 25× real time, 1.6 GB of VRAM) |
| Without it | Everything else works; dictated clips wait and retry for a few minutes, then fail |

## Install

```sh
deploy/whisper/build.sh                       # CUDA if nvcc is found, else CPU
WHISPER_BACKEND=cpu deploy/whisper/build.sh   # force a CPU build
```

The script checks its prerequisites first and stops with the command to
install any that are missing: `cmake`, `gcc`, `git`, `curl`, `ffmpeg` (which
decodes the phone's WebM/Opus or AAC) and, for a GPU build, the CUDA
toolkit. It clones a pinned whisper.cpp release, builds `whisper-server`
statically, installs it to `~/.local/bin`, downloads the model to
`~/.local/share/passporter/whisper/`, and enables the unit. Overrides:
`WHISPER_MODEL`, `CUDA_ARCH`, `WHISPER_REF`. The unit transcribes English;
see [Deploy › Transcription](../deploy.md#build-and-install) for other
languages.

## Check

```sh
journalctl --user -u whisper-server -f      # the device, then "listening"
curl -s 127.0.0.1:8178/inference -F file=@clip.wav -F response_format=json
passporter doctor                           # the whisper line
```

## Bridge settings

The `[audio]` table; every key is optional:

```toml
[audio]
whisper = "127.0.0.1:8178"   # must be loopback; the bridge refuses anything else
retention_hours = 168        # keep transcribed audio this long; 0 deletes it once transcribed
max_upload_mib = 25          # per clip
max_duration_secs = 300      # per clip
```

## Retention

A sweep runs every 10 minutes. Transcribed audio is deleted
`retention_hours` after it finished; its record and transcript go after the
same time but never sooner than 24 hours, so a phone that was offline can
still fetch the transcript. A failed clip keeps its audio for a retry until
its record goes. Queued clips are never swept.

Upload protocol, retries and limits in full:
[Deploy › Transcription](../deploy.md#transcription-whisper-server).
