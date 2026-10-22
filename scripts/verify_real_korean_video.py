#!/usr/bin/env python3
"""Compare source/export video samples at the exact reviewed-cut timeline mapping."""
import argparse
import json
import math
import subprocess
from pathlib import Path

import numpy as np

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('root', type=Path)
parser.add_argument('--minutes', type=int, choices=[1, 10, 60], required=True)
parser.add_argument('--phase', required=True)
args = parser.parse_args()
root = args.root.resolve()
out = root / f'{args.phase}-{args.minutes}min'
source = root / f'real-korean-{args.minutes}min.mp4'
movie = out / 'edited.mp4'
report = json.loads((out / 'report.json').read_text())
candidates = json.loads((out / 'workflow-candidates.json').read_text())['candidates']
selected = set(report['workflow']['selected'])
cuts = []
for c in sorted((c for c in candidates if c['id'] in selected), key=lambda c: c['start']):
    if cuts and c['start'] <= cuts[-1][1]: cuts[-1][1] = max(cuts[-1][1], c['end'])
    else: cuts.append([c['start'], c['end']])


def metadata(path):
    data = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_streams', '-of', 'json', str(path)]))
    return data['streams'][0]


original = metadata(source)
exported = metadata(movie)
assert (original['width'], original['height']) == (exported['width'], exported['height'])
assert original['r_frame_rate'] == exported['r_frame_rate'] == '24/1', 'This fixed-corpus oracle uses the 24fps frame grid'
ticks = 254016000000
frame_ticks = ticks // 24
duration = float(original['duration'])


def frame(path, time):
    raw = subprocess.check_output(['ffmpeg', '-v', 'error', '-ss', f'{time:.9f}', '-i', str(path), '-frames:v', '1', '-pix_fmt', 'rgb24', '-f', 'rawvideo', '-'])
    assert len(raw) == original['width'] * original['height'] * 3
    return np.frombuffer(raw, dtype=np.uint8).astype(float)


samples = []
for time in sorted(set([2, 30, duration / 2, duration - 30, duration - 2])):
    if not 0 <= time < duration: continue
    at = round(time * 24) * frame_ticks
    if any(a <= at < b for a, b in cuts): continue
    removed = sum(b - a for a, b in cuts if b <= at)
    source_time, output_time = at / ticks, (at - removed) / ticks
    x, y = frame(source, source_time), frame(movie, output_time)
    mse = float(np.mean((x - y) ** 2))
    psnr = 10 * math.log10(255 ** 2 / mse) if mse else None
    samples.append({'sourceSeconds': source_time, 'outputSeconds': output_time, 'meanAbsoluteRgbError': float(np.mean(abs(x - y))), 'pixelExact': mse == 0, 'psnrDb': psnr, 'passed': mse == 0 or psnr >= 35})
summary = {'method': 'Independent RGB decoding at five source-frame times mapped through selected frame-aligned cuts; minimum PSNR 35dB. This plain-color corpus is not a camera, HDR, effects, or 4K fidelity benchmark.', 'samples': samples, 'passed': bool(samples) and all(s['passed'] for s in samples)}
(out / 'video-sample-check.json').write_text(json.dumps(summary, indent=2))
print(json.dumps(summary, indent=2))
assert summary['passed']
