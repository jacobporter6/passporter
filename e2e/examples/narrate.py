# /// script
# requires-python = ">=3.10,<3.13"
# dependencies = ["kokoro==0.9.4", "soundfile", "numpy"]
# ///
"""Speaks the demo's narration with Kokoro's af_bella voice.

Reads target/demo/narration/lines.tsv (written by `just demo`) and writes
NN.wav beside it, one per scene. Run from the repo root:

    uv run e2e/examples/narrate.py [narration-dir]

then `just demo` again to record with the voice. The first run downloads
the Kokoro model and a spaCy English model.
"""

import pathlib
import sys

import numpy as np
import soundfile as sf
from kokoro import KPipeline

out = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/demo/narration")
pipeline = KPipeline(lang_code="a", repo_id="hexgrad/Kokoro-82M")  # American English
for row in (out / "lines.tsv").read_text().splitlines():
    n, text = row.split("\t", 1)
    audio = [chunk.audio for chunk in pipeline(text, voice="af_bella", speed=1.0)]
    sf.write(out / f"{n}.wav", np.concatenate(audio), 24000)
    print(n, text)
