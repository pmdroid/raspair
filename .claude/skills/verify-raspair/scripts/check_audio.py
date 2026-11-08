#!/usr/bin/env python3
import argparse
import array
import json
import math
import struct
import sys
import wave

SILENCE_DBFS = -50.0
BLOCK_S = 0.01
WINDOW_S = 0.1
PURITY_MIN = 0.9


def wrap_raw(raw_path, start, end, out_path, rate=48000, channels=2):
    frame = 2 * channels
    start -= start % frame
    end -= end % frame
    with open(raw_path, "rb") as f:
        f.seek(start)
        data = f.read(max(0, end - start))
    with wave.open(out_path, "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(data)


def read_wav(path):
    with open(path, "rb") as f:
        blob = f.read()
    if blob[:4] != b"RIFF" or blob[8:12] != b"WAVE":
        raise ValueError("not a RIFF/WAVE file")
    pos, fmt, data = 12, None, None
    while pos + 8 <= len(blob):
        cid, size = blob[pos:pos + 4], struct.unpack("<I", blob[pos + 4:pos + 8])[0]
        body = pos + 8
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", blob[body:body + 16])
        elif cid == b"data":
            data = blob[body:] if size in (0, 0xFFFFFFFF) or body + size > len(blob) else blob[body:body + size]
            break
        pos = body + size + (size & 1)
    if fmt is None or data is None:
        raise ValueError("missing fmt or data chunk")
    channels, rate, bits = fmt[1], fmt[2], fmt[5]
    width = bits // 8
    data = data[: len(data) - len(data) % (width * channels)]
    if bits == 16:
        ints, scale = array.array("h", data), 32768.0
    elif bits == 32:
        ints, scale = array.array("i", data), 2147483648.0
    elif bits == 24:
        ints = [int.from_bytes(data[i:i + 3], "little", signed=True) for i in range(0, len(data), 3)]
        scale = 8388608.0
    else:
        raise ValueError(f"unsupported bits per sample: {bits}")
    samples = [[ints[i] / scale for i in range(ch, len(ints), channels)] for ch in range(channels)]
    return rate, bits, samples


def dbfs(energy, n):
    if n == 0 or energy <= 0:
        return -120.0
    return 10 * math.log10(energy / n)


def goertzel_power(xs, freq, rate):
    coeff = 2 * math.cos(2 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in xs:
        s1, s2 = x + coeff * s1 - s2, s1
    return s1 * s1 + s2 * s2 - coeff * s1 * s2


def analyse(path, expect, level_db, level_tol, min_seconds, max_seconds):
    rate, bits, chans = read_wav(path)
    total = len(chans[0]) / rate if chans else 0.0
    block = int(rate * BLOCK_S)
    window = int(rate * WINDOW_S)
    limit = int(rate * max_seconds)
    chans = [c[:limit] for c in chans]
    n = len(chans[0]) if chans else 0
    loud = []
    for b in range(0, n - block + 1, block):
        level = min(dbfs(sum(x * x for x in c[b:b + block]), block) for c in chans)
        loud.append(level > SILENCE_DBFS)
    if True not in loud:
        return {"file": path, "rate": rate, "bits": bits, "seconds": round(total, 2),
                "pass": False, "reasons": ["no audio above -50 dBFS"]}
    onset = loud.index(True)
    last = len(loud) - 1 - loud[::-1].index(True)
    gaps = [i for i in range(onset, last + 1) if not loud[i]]
    start, stop = (onset * block) + window, (last + 1) * block - window
    per_channel, bad = [], []
    for ch, samples in enumerate(chans):
        want = expect[ch % len(expect)]
        others = [f for f in expect if f != want]
        energy_sum, count, purity_min, wins = 0.0, 0, 1.0, 0
        for w in range(start, stop - window + 1, window // 2):
            xs = samples[w:w + window]
            energy = sum(x * x for x in xs)
            p_want = goertzel_power(xs, want, rate)
            p_other = max((goertzel_power(xs, f, rate) for f in others), default=0.0)
            purity = 2 * p_want / (window * energy) if energy > 0 else 0.0
            purity_min = min(purity_min, purity)
            energy_sum += energy
            count += len(xs)
            wins += 1
            if p_want <= p_other or purity < PURITY_MIN:
                bad.append({"channel": ch, "at_s": round(w / rate, 2), "purity": round(purity, 3)})
        per_channel.append({"expect_hz": want, "level_dbfs": round(dbfs(energy_sum, count), 2),
                            "purity_min": round(purity_min, 3), "windows": wins})
    active = (last - onset + 1) * BLOCK_S
    reasons = []
    if active < min_seconds:
        reasons.append(f"only {active:.2f}s of audio, need {min_seconds}s")
    if gaps:
        reasons.append(f"{len(gaps)} silent 10ms blocks inside the audio (first at {gaps[0] * BLOCK_S:.2f}s)")
    if bad:
        reasons.append(f"{len(bad)} 100ms windows off-tone or impure (first {bad[0]})")
    for ch, c in enumerate(per_channel):
        if abs(c["level_dbfs"] - level_db) > level_tol:
            reasons.append(f"channel {ch} level {c['level_dbfs']} dBFS, expected {level_db}±{level_tol}")
    return {"file": path, "rate": rate, "bits": bits, "seconds": round(total, 2),
            "onset_s": round(onset * BLOCK_S, 2), "active_seconds": round(active, 2),
            "silent_blocks": len(gaps), "bad_windows": len(bad), "channels": per_channel,
            "pass": not reasons, "reasons": reasons}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("file", nargs="?")
    parser.add_argument("--wrap-raw", nargs=4, metavar=("RAW", "START", "END", "OUT"))
    parser.add_argument("--expect", default="440,660")
    parser.add_argument("--level-db", type=float, default=-15.0)
    parser.add_argument("--level-tolerance", type=float, default=3.0)
    parser.add_argument("--min-seconds", type=float, default=1.0)
    parser.add_argument("--max-seconds", type=float, default=30.0)
    args = parser.parse_args()
    if args.wrap_raw:
        raw, start, end, out = args.wrap_raw
        wrap_raw(raw, int(start), int(end), out)
        return 0
    if not args.file:
        parser.error("FILE is required")
    expect = [float(f) for f in args.expect.split(",")]
    try:
        result = analyse(args.file, expect, args.level_db, args.level_tolerance,
                         args.min_seconds, args.max_seconds)
    except (OSError, ValueError) as e:
        result = {"file": args.file, "pass": False, "reasons": [str(e)]}
    print(json.dumps(result, indent=2))
    return 0 if result["pass"] else 1


if __name__ == "__main__":
    sys.exit(main())
