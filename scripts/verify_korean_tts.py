#!/usr/bin/env python3
"""Mac Korean/English TTS acceptance: preview/create/edit/undo/save/reopen/video export.
Uses the installed Yuna or Samantha voice; ffmpeg/ffprobe are external verification tools only.
"""
import array
import argparse
import json
import math
from pathlib import Path
import subprocess

from verify_korean_cleanup import Engine, ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--voice', choices=['macos-yuna', 'macos-samantha'], default='macos-yuna')
    args = parser.parse_args()
    korean = args.voice == 'macos-yuna'
    language = 'ko-KR' if korean else 'en-US'
    text = '안녕하세요.[pause 1s]한국어 나레이션을 영상에 추가합니다.' if korean else 'Hello.[pause 1s]English narration can be added to a video.'
    changed_text = '다시 만든 한국어 음성입니다.' if korean else 'This is the edited English narration.'
    out = ROOT / ('target/verification-tts' if korean else 'target/verification-tts-en')
    out.mkdir(parents=True, exist_ok=True)
    engine = Engine(ROOT / 'target/debug/filmcraft-cli')
    run = engine.command
    def snapshot(name):
        path = out / (name + '.fcproj')
        run('file.saveCopy', {'path': str(path)})
        return json.loads(path.read_text())['project']
    try:
        run('file.open', {'path': str(ROOT / 'target/verification/before.fcproj')})
        run('timeline.setTrack', {'track': 'a1', 'muted': True})
        before = snapshot('before')
        voices = run('tts.voices')
        assert any(v['id'] == args.voice and v['language'] == language for v in voices['voices'])
        params = {'voice': args.voice, 'text': text, 'pace': 1.0}
        run('tts.preview', dict(params, path=str(out / 'preview.wav')))
        assert snapshot('after-preview') == before, 'preview modified the project'
        created = run('tts.create', dict(params, time=0, dir=str(out)))
        added = snapshot('created')
        assert created['duration'] > 0 and created['track'].lower() != 'a1'
        clip = created['clip']
        edited = run('tts.edit', {'clip': clip, 'text': changed_text})
        changed = snapshot('edited')
        assert changed != added
        # Editing preserves timeline duration, and keeps the full original media for Undo.
        assert edited['duration'] == created['duration']
        run('edit.undo')
        assert snapshot('undo-edit') == added
        run('edit.undo')
        assert snapshot('undo-create') == before
        run('edit.redo')
        run('edit.redo')
        assert snapshot('redo') == changed
        run('file.open', {'path': str(out / 'edited.fcproj')})
        assert run('tts.inspect', {'clip': clip})['narration']['text'] == changed_text
        assert snapshot('reopened') == changed
        movie = out / ('korean-narration.mp4' if korean else 'english-narration.mp4')
        run('file.exportMedia', {'path': str(movie), 'format': 'h264', 'audio': True, 'wait': True})
        probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', str(movie)]))
        assert {s['codec_type'] for s in probe['streams']} == {'video', 'audio'}
        subprocess.run(['ffmpeg', '-v', 'error', '-i', str(movie), '-f', 'null', '-'], check=True)
        samples = array.array('f', subprocess.check_output(['ffmpeg', '-v', 'error', '-i', str(movie), '-vn', '-f', 'f32le', '-']))
        rms = math.sqrt(sum(s*s for s in samples) / len(samples))
        assert rms > .001, 'the isolated narration export is silent'
        report = {'voice': args.voice, 'previewDoesNotEdit': True, 'createEditUndoRedo': True,
                  'saveReopen': True, 'exportDecoded': True, 'isolatedNarrationRms': rms, 'movie': str(movie)}
        (out / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print(json.dumps(report, ensure_ascii=False), flush=True)
    finally:
        engine.close()


if __name__ == '__main__':
    main()
