#!/usr/bin/env python3
"""Smoke-test an installed Release without changing user projects or user preferences.

Uses the existing desktop control driver and FFmpeg only as an external export oracle.
Does not repeat the previous multi-hour 10/60-minute fresh-ASR accuracy evaluation.
"""
import argparse
import hashlib
import json
import os
import plistlib
from pathlib import Path
import subprocess
import time

import verify_long_transcription as driver


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--build', default='18')
    parser.add_argument('--project', type=Path, action='append', required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    binary = args.app / 'Contents/MacOS/FilmCraft'
    info = plistlib.loads((args.app / 'Contents/Info.plist').read_bytes())
    expected = f"FilmCraft 0.6.0 · release · Build {args.build} ({info['FilmCraftBuildDate']}, {info['FilmCraftBuildCommit']})"
    assert subprocess.check_output([str(binary), '--version'], text=True).strip() == expected
    os.environ['FILMCRAFT_QA_APP_BINARY'] = str(binary)
    os.environ['FILMCRAFT_QA_KEEP_AWAKE'] = '1'
    driver.OUT = args.out
    report = {'binary': str(binary), 'binarySHA256': sha(binary), 'freshLongASR': False, 'projects': []}
    desktop = driver.Desktop()
    try:
        state = desktop.call('ui.inspect')
        assert state['build']['number'] == args.build and state['build']['profile'] == 'release', state['build']
        assert state['build']['date'] == info['FilmCraftBuildDate'] and state['build']['commit'] == info['FilmCraftBuildCommit']
        report['build'] = state['build']
        guard = Path(__file__).resolve().parents[1] / 'packaging/macos/installer-scripts/preinstall'
        blocked = subprocess.run(['/bin/sh', str(guard)], capture_output=True, text=True)
        assert blocked.returncode == 1 and 'Save your project' in blocked.stderr, blocked
        report['installerRefusesRunningApp'] = True
        models = desktop.cmd('transcript.models')
        assert models['available'], models
        report['speechCompiled'] = True
        voices = desktop.cmd('tts.voices')
        languages = {entry['id'] for entry in voices['languages']}
        assert {'en-US', 'ko-KR'} <= languages, voices
        report['ttsLanguages'] = sorted(languages)
        desktop.call('ui.focus')
        desktop.call('ui.menu.invoke', {'id': 'app.about'})
        entries = desktop.call('ui.elements', {'prefix': 'about.build'})
        if not entries:
            time.sleep(1)
            entries = desktop.call('ui.elements', {'prefix': 'about.build'})
        assert entries and entries[0]['label'] == report['build']['label'], entries
        try:
            desktop.call('ui.screenshot', {'path': str(args.out / 'about-release.png')})
            report['aboutScreenshot'] = str(args.out / 'about-release.png')
        except AssertionError as error:
            report['screenshotError'] = str(error)
        desktop.call('ui.key', {'key': 'Escape'})
        for index, source in enumerate(args.project):
            folder = args.out / str(index)
            folder.mkdir(exist_ok=True)
            source_hash = sha(source)
            desktop.cmd('file.open', {'path': str(source)})
            before = folder / 'before.fcproj'
            desktop.cmd('file.saveCopy', {'path': str(before)})
            baseline = driver.project(before)
            desktop.cmd('markers.add', {'name': 'Release installer verification'})
            edited = folder / 'edited.fcproj'
            desktop.cmd('file.saveCopy', {'path': str(edited)})
            changed = driver.project(edited)
            assert changed != baseline
            desktop.cmd('edit.undo')
            undo = folder / 'undo.fcproj'
            desktop.cmd('file.saveCopy', {'path': str(undo)})
            assert driver.project(undo) == baseline
            desktop.cmd('edit.redo')
            redo = folder / 'redo.fcproj'
            desktop.cmd('file.saveCopy', {'path': str(redo)})
            assert driver.project(redo) == changed
            desktop.cmd('file.open', {'path': str(edited)})
            reopened = folder / 'reopened.fcproj'
            desktop.cmd('file.saveCopy', {'path': str(reopened)})
            assert driver.project(reopened) == changed
            output = folder / 'export.mp4'
            # Exercise both the beginning and the late seek of the long project.
            start = 0 if index == 0 else 3500
            job = desktop.cmd('file.exportMedia', {'path': str(output), 'format': 'h264', 'audio': True,
                              'range': 'custom', 'startSeconds': start, 'endSeconds': start + 3, 'wait': False})
            deadline = time.monotonic() + 1800
            while True:
                desktop.call('ui.inspect', measure=True)
                status = next(entry for entry in desktop.cmd('jobs.list') if entry['id'] == job['job'])
                if status['finished']:
                    assert not status['result'].get('error'), status
                    break
                assert time.monotonic() < deadline, status
                time.sleep(.5)
            subprocess.run(['ffmpeg', '-v', 'error', '-i', str(output), '-f', 'null', '-'], check=True, timeout=120)
            streams = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-show_streams', '-of', 'json', str(output)]))['streams']
            assert {stream['codec_type'] for stream in streams} >= {'audio', 'video'}
            assert sha(source) == source_hash
            report['projects'].append({'source': str(source), 'sourceSHA256': source_hash, 'sourceUnchanged': True,
                                       'undoRedo': True, 'saveReopen': True, 'exportFullDecode': True,
                                       'exportSeconds': 3, 'exportStartSeconds': start, 'streams': streams})
        report['passed'] = True
    finally:
        desktop.close()
        (args.out / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({'passed': True, 'build': report['build'], 'projects': len(report['projects'])}, ensure_ascii=False))


if __name__ == '__main__':
    main()
