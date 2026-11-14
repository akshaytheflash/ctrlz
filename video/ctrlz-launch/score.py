#!/usr/bin/env python3
"""ctrlz launch film: tactile score from the comp's __events(), one room,
one quiet synthetic bed. 48 kHz stereo, peak -8 dBFS."""
import json, os, sys
import numpy as np
from scipy import signal

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.abspath(os.path.join(HERE, "..", "..", "..", ".agents", "skills", "onetake", "scripts")))
from sfx_palette import *  # noqa

DUR = 22.0
EV = json.load(open(os.path.join(HERE, "events.json")))["events"]
STILLS = [(3.30, 4.05), (7.75, 8.55), (14.10, 15.20), (19.30, 22.00)]

def sstep(a, b, x):
    k = np.clip((x - a) / (b - a), 0, 1)
    return k * k * (3 - 2 * k)

def still_mask(t):
    m = np.zeros_like(t)
    for a, b in STILLS:
        m = np.maximum(m, sstep(a - 0.10, a, t) * (1 - sstep(b, b + 0.10, t)))
    return 1 - m

s = Score(dur=DUR, T60=1.5)
ki = 0
for e in EV:
    k, t, pan, v = e["kind"], e["t"], e.get("pan", 0.0), e.get("v", 0.5)
    if k == "bloom":
        s.place(sub(46, 2.0), t, 0.24, pan, 0.55)
        s.place(air(1.6, 90, 900, 1.2, 0.5), t, 0.26, pan, 0.6)
    elif k == "key":
        f = 152 + (ki * 41) % 74
        s.place(wood(f, 0.085), t, 0.10 + 0.05 * v, pan, 0.16)
        ki += 1
    elif k == "click":
        s.place(wood(224, 0.08), t, 0.34, pan, 0.2)
        s.place(bubble(610, 0.20), t, 0.16, pan, 0.25)
    elif k == "land":
        s.place(sub(58, 0.95), t, 0.20, pan, 0.35)
        s.place(wood(140, 0.12), t, 0.18, pan, 0.2)
        s.place(bubble(430, 0.25), t, 0.12, pan, 0.3)
    elif k == "air":
        s.place(air(1.0, 180, 1900, 1.3, 0.45), t, 0.36, pan, 0.5, pan_to=min(0.7, pan + 0.2))
    elif k == "glass":
        s.place(glass(880, 0.8, 0.6), t, 0.22, pan, 0.45)
    elif k == "hit":
        big = v > 0.8
        s.place(sub(48 if big else 64, 1.3 if big else 0.9), t, 0.16 + 0.12 * v, pan, 0.35)
        s.place(wood(118 if big else 150, 0.12), t, 0.13 + 0.09 * v, pan, 0.2)
        s.place(glass(330 if big else 392, 0.6, 0.5), t, 0.08, pan, 0.5)
    elif k == "bad":
        s.place(sub(70, 0.8), t, 0.18, pan, 0.3)
        s.place(glass(311, 0.7, 0.4), t, 0.10, pan, 0.5)
    elif k == "pass":
        s.place(glass(659, 1.0, 0.6), t, 0.22, pan, 0.5)
        s.place(glass(988, 1.1, 0.4), t + 0.01, 0.12, pan, 0.55)
    elif k == "sub":
        s.place(sub(55, 1.1), t, 0.20, pan, 0.3)
        s.place(glass(330, 0.7, 0.4), t, 0.08, pan, 0.5)
    elif k == "rise":
        s.place(air(1.8, 70, 2800, 1.1, 0.55), t, 0.42, pan, 0.65)
        s.place(sub(46, 1.6), t + 0.3, 0.10, pan, 0.4)
    elif k == "word":
        s.place(glass(659, 1.5, 0.7), t, 0.30, pan, 0.55)
        s.place(glass(988, 1.6, 0.5), t + 0.02, 0.16, pan, 0.6)
        s.place(sub(65, 1.2), t, 0.07, pan, 0.3)
    elif k == "tag":
        s.place(glass(784, 0.9, 0.4), t, 0.08, pan, 0.5)
    elif k == "cta":
        s.place(glass(587, 1.2, 0.5), t, 0.14, pan, 0.5)
        s.place(sub(55, 1.0), t, 0.05, pan, 0.3)
    elif k == "end":
        s.place(sub(43, 1.8), t, 0.10, pan, 0.3)
        s.place(air(1.2, 100, 800, 1.1, 0.5), t, 0.13, pan, 0.4)

sfx = s.mix(peak_db=-8.0)

# the bed: a low room that follows the story's sections
SR = 48000
n = int(SR * DUR)
t = np.arange(n) / SR
CH = [(0.00, 4.30, [55.00, 82.41, 110.00, 164.81]),
      (4.30, 6.55, [49.00, 73.42, 98.00, 146.83]),
      (6.55, 9.05, [55.00, 82.41, 110.00, 164.81]),
      (9.05, 12.20, [65.41, 98.00, 130.81, 196.00]),
      (12.20, 15.35, [55.00, 82.41, 110.00, 164.81]),
      (15.35, 17.05, [73.42, 110.00, 146.83, 220.00]),
      (17.05, 22.20, [55.00, 82.41, 110.00, 138.59, 164.81])]
bed = np.zeros(n)
for a, b, notes in CH:
    env = sstep(a - 0.7, a + 0.7, t) * (1 - sstep(b - 0.7, b + 0.7, t))
    for i, f in enumerate(notes):
        amp = (0.16, 0.10, 0.07, 0.045, 0.03)[i]
        det = 1 + 0.0008 * np.sin(2 * np.pi * 0.11 * t + i)
        bed += amp * np.sin(2 * np.pi * f * det * t + 0.4 * np.sin(2 * np.pi * 0.07 * t + i * 1.7)) * env
    bed += 0.05 * np.sin(2 * np.pi * notes[0] * t) * env
bed = lp(bed, 620)
bed = hp(bed, 45)
bed *= 0.9 + 0.1 * np.sin(2 * np.pi * 0.06 * t)

# one room: send the pad through the same impulse
ir = impulse(1.5)
wet = signal.fftconvolve(bed, ir[:, 0])[:n] * 0.30
bed = bed + wet
bed = np.stack([bed, np.roll(bed, int(0.006 * SR)) * 0.96], 1)

# rests: the pad all but leaves in the stills
gate = 0.05 + 0.95 * still_mask(t)
# duck it under every contact
duck = np.ones(n)
for e in EV:
    if e["kind"] in ("bloom", "land", "click", "hit", "word", "end", "pass", "rise"):
        i0 = int(e["t"] * SR)
        seg = np.arange(0, min(n - i0, int(1.2 * SR))) / SR
        duck[i0:i0 + len(seg)] *= 1 - 0.5 * np.exp(-seg / 0.32)
bed *= (gate * duck)[:, None]

# pad RMS sits ~14 dB under the action
rms = np.sqrt((bed ** 2).mean())
bed *= 10 ** (-28.0 / 20.0) / (rms + 1e-9)

mix = sfx + bed
low = lp(mix, 120)
mix = low * 0.52 + (mix - low) * 1.30
mix = hp(mix, 38)
mix = np.tanh(mix * 1.15) / np.tanh(1.15)
mix = mix / (np.abs(mix).max() + 1e-9) * 10 ** (-8.0 / 20.0)

import wave
with wave.open(os.path.join(HERE, "sfx.wav"), "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR)
    w.writeframes((mix[:int(SR * DUR)] * 32767).astype(np.int16).tobytes())
print("sfx.wav", len(EV), "events | peak", round(20 * np.log10(np.abs(mix).max()), 1), "dBFS | pad rms", round(20 * np.log10(np.sqrt((bed ** 2).mean()) + 1e-9), 1))
