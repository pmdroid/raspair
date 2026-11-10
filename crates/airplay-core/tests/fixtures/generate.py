import math
import sys
import wave
from pathlib import Path

import av
import numpy as np

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent)
FRAMES = 4800


def pattern(n, c):
    return (((n * 2 + c) * 2654435761) & 0xFFFFFFFF) >> 8


def pcm24():
    return np.array([[pattern(n, c) - 0x800000 for c in range(2)] for n in range(FRAMES)], dtype=np.int32)


def write_wav(path, samples):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(3)
        w.setframerate(48000)
        w.writeframes(b"".join(int(v).to_bytes(3, "little", signed=True) for v in samples.reshape(-1)))


def encode(path, codec, rate, layout, fmt, frames, tags=None, extra=None):
    out = av.open(str(path), "w")
    for k, v in (tags or {}).items():
        out.metadata[k] = v
    s = out.add_stream(codec, rate=rate, layout=layout)
    s.format = fmt
    for k, v in (extra or {}).items():
        setattr(s.codec_context, k, v)
    chunk = 1152
    for start in range(0, len(frames), chunk):
        part = frames[start:start + chunk]
        planar = fmt.endswith("p")
        f = av.AudioFrame.from_ndarray(part.T.copy() if planar else part.reshape(1, -1), format=fmt, layout=layout)
        f.sample_rate = rate
        f.pts = start
        out.mux(s.encode(f))
    out.mux(s.encode(None))
    out.close()


def sine(rate, seconds, channels):
    n = np.arange(int(rate * seconds))
    tone = 0.5 * np.sin(2 * math.pi * 440 * n / rate).astype(np.float32)
    return np.stack([tone] * channels, axis=1)


ref = pcm24()
write_wav(OUT / "pcm24_48k_stereo.wav", ref)
encode(OUT / "pcm24_48k_stereo.flac", "flac", 48000, "stereo", "s32", ref << 8, extra={"bits_per_raw_sample": 24})
encode(OUT / "alac24_48k_stereo.m4a", "alac", 48000, "stereo", "s32p", ref << 8, extra={"bits_per_raw_sample": 24})
tags = {"title": "Fixture Tone", "artist": "raspair"}
encode(OUT / "tone_44k_stereo.mp3", "libmp3lame", 44100, "stereo", "fltp", sine(44100, 0.5, 2), tags=tags)
encode(OUT / "tone_48k_mono.m4a", "aac", 48000, "mono", "fltp", sine(48000, 0.5, 1), tags=tags)
