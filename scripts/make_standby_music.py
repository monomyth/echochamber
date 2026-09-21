#!/usr/bin/env python3
"""Generate a looping lounge/elevator bed (stdlib only)."""

from __future__ import annotations

import math
import random
import struct
import wave
from pathlib import Path

SR = 48000
BPM = 60.0
BEAT = 60.0 / BPM
BAR = BEAT * 4.0
BARS = 8
DURATION = BAR * BARS  # 32s
N = int(SR * DURATION)


def midi(n: float) -> float:
    return 440.0 * (2.0 ** ((n - 69.0) / 12.0))


# Cmaj7, Am7, Fmaj7, G7 — two cycles, hotel-lobby ii/I-ish
CHORDS = [
    [48, 52, 55, 59],  # C3 E3 G3 B3
    [45, 48, 52, 55],  # A2 C3 E3 G3
    [41, 45, 48, 52],  # F2 A2 C3 E3
    [43, 47, 50, 53],  # G2 B2 D3 F3
    [48, 52, 55, 59],
    [45, 48, 52, 55],
    [41, 45, 48, 52],
    [43, 47, 50, 53],
]

MELODY = [
    # sparse, on the off-bars
    (0.0, None),
    (BAR * 0.5, 76),  # E5
    (BAR * 1.5, 79),  # G5
    (BAR * 2.25, 74),  # D5
    (BAR * 3.5, 72),  # C5
    (BAR * 4.5, 76),
    (BAR * 5.75, 71),  # B4
    (BAR * 6.5, 72),
    (BAR * 7.25, 74),
]


def sine(f: float, t: float, ph: float = 0.0) -> float:
    return math.sin(2.0 * math.pi * f * t + ph)


def soft(x: float) -> float:
    return math.tanh(x)


def lin(t: float, t0: float, t1: float) -> float:
    if t1 <= t0:
        return 1.0
    if t < t0:
        return 0.0
    if t > t1:
        return 1.0
    return (t - t0) / (t1 - t0)


def main() -> None:
    rng = random.Random(7)
    left = [0.0] * N
    right = [0.0] * N

    for i in range(N):
        t = i / SR
        bar_i = min(int(t / BAR), BARS - 1)
        chord = CHORDS[bar_i]
        local = t - bar_i * BAR

        # pad: slow swell each bar, slight stereo detune
        pad_env = lin(local, 0.0, 0.9) * (1.0 - 0.35 * lin(local, BAR - 1.2, BAR))
        trem = 0.85 + 0.15 * sine(0.35, t)
        pad_l = 0.0
        pad_r = 0.0
        for k, m in enumerate(chord):
            f = midi(m)
            det = 0.08 + 0.04 * k
            pad_l += sine(f - det, t, k) * (0.22 if k else 0.16)
            pad_r += sine(f + det, t, k + 0.7) * (0.22 if k else 0.16)
        pad_l *= pad_env * trem
        pad_r *= pad_env * trem

        # bass on 1 and 3
        beat = local / BEAT
        bass_on = (beat % 2.0) < 1.15
        bass_env = 0.0
        if bass_on:
            bpos = beat % 2.0
            bass_env = lin(bpos, 0.0, 0.04) * (1.0 - lin(bpos, 0.7, 1.15))
        root = midi(chord[0] - 12)
        bass = bass_env * (0.55 * sine(root, t) + 0.12 * sine(root * 2, t))

        left[i] += pad_l * 0.55 + bass * 0.7
        right[i] += pad_r * 0.55 + bass * 0.55

    # melody: quiet Rhodes-ish (fundamental + 5th + bell)
    for t0, note in MELODY:
        if note is None:
            continue
        f = midi(note)
        dur = 1.8
        n0 = int(t0 * SR)
        n1 = min(N, int((t0 + dur) * SR))
        ph = rng.random() * 0.4
        for i in range(n0, n1):
            u = (i - n0) / SR
            env = lin(u, 0.0, 0.02) * (1.0 - lin(u, 0.9, dur))
            env *= math.exp(-u * 1.1)
            tone = (
                0.55 * sine(f, u, ph)
                + 0.22 * sine(f * 2.005, u, ph)
                + 0.08 * sine(f * 4.02, u)
            )
            pan = 0.35
            left[i] += tone * env * (0.18 * (1.0 - pan))
            right[i] += tone * env * (0.18 * (1.0 + pan) / 2.0)

    # Short edge fades so looping the file does not click.
    fade = int(0.08 * SR)
    for i in range(fade):
        w = i / fade
        left[i] *= w
        right[i] *= w
        left[N - 1 - i] *= w
        right[N - 1 - i] *= w

    peak = 1e-9
    for i in range(N):
        peak = max(peak, abs(left[i]), abs(right[i]))
    gain = 0.28 / peak  # leave headroom; this sits under a live mix

    out = Path(__file__).resolve().parent.parent / "assets" / "standby.wav"
    out.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(out), "w") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        frames = bytearray()
        for i in range(N):
            l = int(max(-1.0, min(1.0, soft(left[i] * gain * 1.4))) * 32767)
            r = int(max(-1.0, min(1.0, soft(right[i] * gain * 1.4))) * 32767)
            frames += struct.pack("<hh", l, r)
        w.writeframes(frames)
    print(f"wrote {out} ({DURATION:.1f}s)")


if __name__ == "__main__":
    main()
