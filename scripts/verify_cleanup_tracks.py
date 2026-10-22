#!/usr/bin/env python3
"""Exercise reviewed cleanup in the actual desktop with overlapping and locked tracks.

Use a saved real-model project and its human reference. Reference labels are used only
for assertions and explicit QA selection, never supplied to recognition or editing.
"""
import argparse
import copy
import hashlib
import json
import subprocess
import time
import numpy as np
from pathlib import Path

import verify_long_transcription as desktop
from evaluate_korean_asr import evaluate

TICKS = 254016000000


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('--minutes', type=int, choices=[10, 60], required=True)
    parser.add_argument('--project', type=Path, required=True)
    parser.add_argument('--phase', default='review14')
    parser.add_argument('--hardware-encoding', choices=['off', 'auto'], default='off')
    args = parser.parse_args()
    root = args.root.resolve()
    out = root / f'{args.phase}-{args.minutes}min'
    out.mkdir(exist_ok=True)
    desktop.OUT = out
    source = root / f'real-korean-{args.minutes}min.mp4'
    digest = hashlib.file_digest(source.open('rb'), 'sha256').hexdigest()
    reference = json.loads((root / f'reference-{args.minutes}min.json').read_text())
    fixture = json.loads(args.project.read_text())
    results = []
    d = desktop.Desktop()

    def sequence(project):
        return next(item['kind']['Sequence'] for item in project['project']['items'].values() if 'Sequence' in item['kind'])

    def save(name):
        path = out / f'{name}.fcproj'
        d.cmd('file.saveCopy', {'path': str(path)})
        return json.loads(path.read_text())['project']

    def open_fixture(name, data):
        path = out / f'{name}-input.fcproj'
        path.write_text(json.dumps(data, ensure_ascii=False))
        d.cmd('file.open', {'path': str(path)})

    def export(params):
        job = d.cmd('file.exportMedia', dict(params, wait=False))
        deadline = time.monotonic() + 8 * 3600
        last = time.monotonic()
        while True:
            state = next(j for j in d.cmd('jobs.list') if j['id'] == job['job'])
            if state['finished']:
                assert not state['result'].get('error'), state
                return state['result']
            assert time.monotonic() < deadline, state
            if time.monotonic() - last >= 30:
                print(json.dumps({'export': params['path'], 'progress': state['progress'], 'status': state['status']}), flush=True)
                last = time.monotonic()
            time.sleep(2)

    def add_audio(data, offset=0, empty=False, prefix_only=False):
        project = data['project']
        track = copy.deepcopy(sequence(data)['audio_tracks'][0])
        track['id'] = project['next_id']; project['next_id'] += 1
        track['name'] = 'QA protected audio'
        track['items'] = [] if empty else track['items'][:1]
        for clip in track['items']:
            clip['id'] = project['next_id']; project['next_id'] += 1
            clip['link'] = None
            clip['source_in'] += offset
            clip['duration'] -= offset
            if prefix_only:
                clip['duration'] = TICKS
        sequence(data)['audio_tracks'].append(track)
        return track

    try:
        open_fixture('baseline', fixture)
        initial = d.cmd('transcript.previewCleanup')
        initial_accuracy = evaluate(reference, d.cmd('transcript.inspect'), initial)
        approved = [c for c in initial_accuracy['fillers']['details'] if c['actionable']]
        assert approved and all(c['lexicalHit'] and c['nonFillerOverlapSeconds'] <= 1e-6 for c in approved), approved
        target = next(c for c in initial['candidates'] if c['id'] == approved[0]['id'])
        target_start, target_end = target['start'] / TICKS, target['end'] / TICKS
        ordinary = [w for w in reference['words'] if w['text'].strip(' ,.?!') not in {'음', '어', '으음', '어어'}]
        overlap = lambda offset: sum(max(0, min(target_end + offset, w['end']) - max(target_start + offset, w['start'])) for w in ordinary)
        offset = 1.25 if args.minutes == 10 else max([n / 4 for n in range(1, 13)], key=overlap)
        assert overlap(offset) > .03, 'Adversarial second track must contain ordinary speech at the cut'
        window_start = max(0, int(target_start) - 1)
        window_end = window_start + 5
        for name in ['locked-audio', 'locked-video', 'overlapping-different-speech', 'custom-primary-channel']:
            data = copy.deepcopy(fixture)
            q = sequence(data)
            if name == 'locked-audio': q['audio_tracks'][0]['locked'] = True
            elif name == 'locked-video': q['video_tracks'][0]['locked'] = True
            elif name == 'custom-primary-channel': q['audio_tracks'][0]['items'][0]['source_channels'] = [0]
            else: add_audio(data, offset=round(offset * TICKS))
            open_fixture(name, data)
            before = save(name + '-before')
            if name == 'overlapping-different-speech':
                export({'path': str(out / 'protected-before.wav'), 'format': 'wav', 'range': 'custom', 'startSeconds': window_start, 'endSeconds': window_end})
            preview = d.cmd('transcript.previewCleanup')
            selected = next(c for c in preview['candidates'] if c['kind'] == 'filler' and c['start'] == target['start'] and c['text'] == target['text'])
            assert selected['actionable'] is False, (name, selected)
            visual = None
            try:
                d.cmd('transcript.applyCleanup', {'token': preview['token'], 'params': preview['params'], 'selected': [selected['id']]})
            except AssertionError as error:
                assert 'not actionable' in str(error).lower() or selected['reason'] in str(error), error
            else:
                raise AssertionError(name + ' did not reject unsafe cleanup')
            assert save(name + '-rejected') == before
            # Save/reopen must preserve the entire rejected transaction's input project.
            d.cmd('file.open', {'path': str(out / (name + '-rejected.fcproj'))})
            assert save(name + '-reopened') == before
            if name == 'overlapping-different-speech':
                export({'path': str(out / 'protected-after.wav'), 'format': 'wav', 'range': 'custom', 'startSeconds': window_start, 'endSeconds': window_end})
                def decoded(path):
                    return subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(path), '-vn', '-ac', '1', '-ar', '48000', '-f', 'f32le', '-'])
                original_audio = decoded(out / 'protected-before.wav')
                assert original_audio and decoded(out / 'protected-after.wav') == original_audio, 'Rejected cleanup changed exported audio'
                raw = subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(source), '-t', str(window_end + offset + 1), '-vn', '-ac', '1', '-ar', '48000', '-f', 'f32le', '-'])
                samples = np.frombuffer(raw, dtype='<f4').astype(float)
                expected = samples[window_start * 48000:window_end * 48000] + samples[round((window_start + offset) * 48000):round((window_end + offset) * 48000)]
                actual = np.frombuffer(original_audio, dtype='<f4').astype(float)
                correlations = []
                for lag in range(-3, 4):
                    a = max(0, -lag); b = min(len(expected), len(actual) - lag)
                    x = expected[a:b]; y = actual[a + lag:b + lag]
                    x = x - x.mean(); y = y - y.mean()
                    correlations.append(float(np.dot(x, y) / (np.linalg.norm(x) * np.linalg.norm(y))))
                protected_words = [w for w in ordinary if min(target_end + offset, w['end']) > max(target_start + offset, w['start'])]
                audio_check = {'beforeAfterExact': True, 'expectedTwoSourceSumCorrelation': max(correlations), 'sampleRate': 48000, 'maximumLagSamples': 3, 'sourceOffsetsSeconds': [0, offset], 'timelineWindowSeconds': [window_start, window_end], 'protectedWords': protected_words}
                (out / 'protected-audio-check.json').write_text(json.dumps(audio_check, indent=2))
                assert audio_check['expectedTwoSourceSumCorrelation'] >= .95, audio_check
            results.append({'case': name, 'blockedBeforeMutation': True, 'saveReopenExact': True, 'reason': selected['reason'], 'visual': visual})

        data = copy.deepcopy(fixture)
        add_audio(data)  # aligned duplicate speech
        empty_locked = add_audio(data, empty=True)
        empty_locked['locked'] = True
        prefix = add_audio(data, prefix_only=True)
        prefix_before = copy.deepcopy(prefix)
        open_fixture('aligned-duplicates-with-empty-lock-and-prefix', data)
        before = save('before')
        transcript = d.cmd('transcript.inspect')
        (out / 'transcript.json').write_text(json.dumps(transcript, ensure_ascii=False, indent=2))
        preview = d.cmd('transcript.previewCleanup', {'minSeconds': .8, 'keepSeconds': .15})
        accuracy = evaluate(reference, transcript, preview)
        approved = [c for c in accuracy['fillers']['details'] if c['actionable']]
        assert approved, 'No checked lexical fillers'
        assert all(c['lexicalHit'] and c['nonFillerOverlapSeconds'] <= 1e-6 for c in approved), approved
        selected = [c['id'] for c in approved]
        (out / 'workflow-candidates.json').write_text(json.dumps(preview, ensure_ascii=False, indent=2))
        (out / 'accuracy.json').write_text(json.dumps(accuracy, ensure_ascii=False, indent=2))
        d.cmd('transcript.applyCleanup', {'token': preview['token'], 'params': preview['params'], 'selected': selected})
        after = save('edited')
        q = next(item['kind']['Sequence'] for item in after['items'].values() if 'Sequence' in item['kind'])
        timing = lambda track: [(c['start'], c['duration'], c['source_in']) for c in track['items']]
        assert timing(q['audio_tracks'][0]) == timing(q['audio_tracks'][1])
        assert q['audio_tracks'][3] == prefix_before
        d.cmd('edit.undo'); assert save('undo') == before
        d.cmd('edit.redo'); assert save('redo') == after
        d.cmd('file.open', {'path': str(out / 'edited.fcproj')}); assert save('reopened') == after
        export({'path': str(out / 'edited.mp4'), 'format': 'h264', 'audio': True, 'hardwareEncoding': args.hardware_encoding})
        probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-count_frames', '-show_streams', '-of', 'json', str(out / 'edited.mp4')]))
        video = next(s for s in probe['streams'] if s['codec_type'] == 'video')
        assert int(video['nb_read_frames']) == d.cmd('sequence.inspect')['durationFrames']
        assert any(s['codec_type'] == 'audio' for s in probe['streams'])
        subprocess.run(['ffmpeg', '-v', 'error', '-i', str(out / 'edited.mp4'), '-f', 'null', '-'], check=True)
        assert hashlib.file_digest(source.open('rb'), 'sha256').hexdigest() == digest
        results.append({'case': 'aligned-duplicates-empty-lock-prefix', 'applied': True, 'tracksRemainAligned': True, 'prefixTrackUnchanged': True, 'undoRedoExact': True, 'saveReopenExact': True})
        report = {'binaryExecutable': str(d.binary), 'binarySHA256': hashlib.file_digest(d.binary.open('rb'), 'sha256').hexdigest(), 'minutes': args.minutes, 'method': 'Saved primary recognition; real audio boundary/filler rechecks followed by desktop multitrack cleanup', 'primaryRecognitionRepeated': False, 'sourceSHA256': digest, 'sourcePreserved': True, 'trackCases': results, 'workflow': {'selected': selected, 'undoRedo': True, 'saveReopen': True, 'exportFrames': int(video['nb_read_frames']), 'exportSeconds': float(video['duration']), 'exportFullDecode': True, 'hardwareEncodingRequested': args.hardware_encoding}}
        (out / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print(json.dumps(report, ensure_ascii=False), flush=True)
    finally:
        d.close()


if __name__ == '__main__':
    main()
