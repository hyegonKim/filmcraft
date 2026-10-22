#!/usr/bin/env python3
"""Real-model smoke test. Supply an original Korean MP4 containing a long pause.
Requires a whisper-enabled filmcraft-cli, installed whisper-small, ffprobe and ffmpeg.
All output is written below target/verification; no input media is modified.
"""
import argparse
import array
import json
import math
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TICKS = 254016000000

class Engine:
    def __init__(self, binary):
        data = ROOT / 'target/verification/data'
        data.mkdir(parents=True, exist_ok=True)
        models = data / 'models'
        if not models.exists():
            models.symlink_to(Path.home() / 'Library/Application Support/FilmCraft/models', target_is_directory=True)
        env = dict(os.environ, FILMCRAFT_DATA_DIR=str(data))
        self.stderr = (ROOT / 'target/verification/mcp.log').open('w')
        self.p = subprocess.Popen([str(binary), 'mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=self.stderr, text=True, env=env)
        self.counter = 0
        self.request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                   'clientInfo': {'name': 'korean-cleanup-verification', 'version': '1'}})
        self.p.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.p.stdin.flush()
    def request(self, method, params):
        self.counter += 1
        ident = self.counter
        self.p.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': ident, 'method': method, 'params': params}) + '\n')
        self.p.stdin.flush()
        for line in self.p.stdout:
            response = json.loads(line)
            if response.get('id') == ident:
                if 'error' in response:
                    raise RuntimeError(response['error'])
                return response['result']
        raise RuntimeError('FilmCraft MCP exited: ' + str(self.p.poll()))
    def command(self, ident, params=None):
        result = self.request('tools/call', {'name': 'command_run', 'arguments': {'id': ident, 'params': params or {}}})
        if result.get('isError'):
            raise RuntimeError(result)
        texts = [c['text'] for c in result.get('content', []) if c.get('type') == 'text']
        return json.loads(texts[0]) if texts else result.get('structuredContent')
    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.terminate()
            self.p.wait(timeout=10)
        self.stderr.close()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('media', type=Path)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/filmcraft-cli')
    parser.add_argument('--transcribed-project', type=Path, help='reuse a previously verified real-model transcript')
    args = parser.parse_args()
    out = ROOT / 'target/verification'
    out.mkdir(parents=True, exist_ok=True)
    engine = Engine(args.binary.resolve())
    try:
        def run(command, params=None):
            return engine.command(command, params)
        if args.transcribed_project:
            run('file.open', {'path': str(args.transcribed_project.resolve())})
            print('Reusing previously verified real Korean transcription.', flush=True)
        else:
            run('file.newProject', {'name': 'Korean cleanup verification'})
            imported = run('file.import', {'paths': [str(args.media.resolve())]})
            assert not imported.get('errors'), imported
            item = imported['items'][0]
            run('file.newSequenceFromClip', {'items': [item]})
            print('Transcribing real Korean audio locally…', flush=True)
            run('transcript.generate', {'items': [item], 'language': 'ko', 'model': 'whisper-small', 'diarize': False, 'wait': True})
        transcript = run('transcript.inspect')
        (out / 'transcript.json').write_text(json.dumps(transcript, ensure_ascii=False, indent=2))
        spoken = ' '.join(w['text'] for w in transcript['words'])
        assert sum('\uac00' <= c <= '\ud7a3' for c in spoken) >= 10, spoken
        run('file.saveAs', {'path': str(out / 'before.fcproj')})
        before = run('sequence.inspect')
        original = json.loads((out / 'before.fcproj').read_text())['project']
        preview = run('transcript.previewCleanup', {'minSeconds': 0.8, 'keepSeconds': 0.15})
        assert preview['candidates'], 'No pause/filler candidate found: ' + spoken
        assert run('sequence.inspect')['duration'] == before['duration'], 'preview mutated the timeline'
        # Explicitly select one pause and, when recognised, one filler. Leave other candidates.
        selected = []
        for kind in ['pause', 'filler']:
            c = next((c for c in preview['candidates'] if c['kind'] == kind and c.get('actionable', True)), None)
            if c:
                selected.append(c['id'])
        result = run('transcript.applyCleanup', {'token': preview['token'], 'params': preview['params'], 'selected': selected})
        after = run('sequence.inspect')
        assert after['duration'] == before['duration'] - result['ticks']
        run('file.saveCopy', {'path': str(out / 'after.fcproj')})
        run('edit.undo')
        run('file.saveCopy', {'path': str(out / 'undo.fcproj')})
        assert json.loads((out / 'undo.fcproj').read_text())['project'] == original, 'one Undo did not restore all cuts'
        run('edit.redo')
        assert run('sequence.inspect')['duration'] == after['duration']
        run('file.open', {'path': str(out / 'after.fcproj')})
        assert run('sequence.inspect')['duration'] == after['duration'], 'save/reopen changed timing'
        print('Exporting the edited project…', flush=True)
        movie = out / 'korean-cleanup.mp4'
        run('file.exportMedia', {'path': str(movie), 'format': 'h264', 'audio': True, 'wait': True})
        probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(movie)]))
        video = next(s for s in probe['streams'] if s['codec_type'] == 'video')
        assert int(video['nb_frames']) == after['durationFrames']
        assert abs(float(video['duration']) - after['duration'] / TICKS) < 1 / 24
        subprocess.run(['ffmpeg', '-v', 'error', '-i', str(movie), '-f', 'null', '-'], check=True)
        samples = array.array('f', subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(movie), '-vn', '-f', 'f32le', '-']))
        rms = math.sqrt(sum(x*x for x in samples) / len(samples))
        assert rms > 0.001, 'exported audio is silent'
        report = {'transcript': spoken, 'candidates': preview['candidates'], 'selected': selected,
                  'beforeSeconds': before['duration']/TICKS, 'afterSeconds': after['duration']/TICKS,
                  'oneUndoRestoredProject': True, 'saveReopenPassed': True, 'asrReused': bool(args.transcribed_project),
                  'videoFrames': int(video['nb_frames']), 'audioRms': rms, 'export': str(movie)}
        (out / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print(json.dumps({k: v for k, v in report.items() if k != 'candidates'}, ensure_ascii=False), flush=True)
    finally:
        engine.close()

if __name__ == '__main__':
    main()
