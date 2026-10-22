#!/usr/bin/env python3
"""Prepare independently annotated Seoul Corpus interview, keeping licensed media outside the repo."""
from pathlib import Path
import argparse,hashlib,json,re,subprocess,zipfile
SOURCE='https://openslr.org/113/'
def intervals(text,tier):
    for block in re.split(r'    item \[\d+\]:',text)[1:]:
        if re.search(r'name = "(.*?)"',block)[1]!=tier:continue
        rows=[{'start':float(a),'end':float(b),'text':t.replace('""','"')} for a,b,t in re.findall(r'intervals \[\d+\]:\s*xmin = ([\d.eE+-]+)\s*xmax = ([\d.eE+-]+)\s*text = "((?:""|[^"])*)"',block,re.S)]
        if any(re.search('[가-힣]',r['text']) for r in rows):return rows
    raise ValueError('missing Korean orthographic tier')
def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('data',type=Path);p.add_argument('--speaker',default='s12m23f');a=p.parse_args();root=a.data.resolve()
    all_words=[];excluded=[];silences=[];offset=0;segments=[]
    with zipfile.ZipFile(root/'labels.zip') as labels:
        for i in range(1,7):
            name=f'{a.speaker}{i}';media=root/(name+'.flac');assert media.exists(),media
            text=labels.read(name+'.TextGrid').decode('utf-16')
            duration=float(re.search(r'xmax = ([\d.]+)',text)[1])
            probe=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_format','-of','json',str(media)]))
            assert abs(float(probe['format']['duration'])-duration)<.01,'label/audio duration mismatch'
            for w in intervals(text,'pWord.ortho.'):
                w=dict(w,start=w['start']+offset,end=w['end']+offset)
                if w['text']=='<SIL>':silences.append(w)
                if '<' in w['text']:
                    if any(k in w['text'] for k in ['IVER','PRIVATE.INFO','UNKNOWN','LAUGH-']):excluded.append(w)
                elif w['text'].strip():all_words.append(w)
            segments.append({'file':media.name,'seconds':duration,'offset':offset,'sha256':hashlib.file_digest(media.open('rb'),'sha256').hexdigest()});offset+=duration
    assert offset>=3600,'cannot manufacture a 60-minute continuous recording from a shorter interview'
    (root/'concat.txt').write_text(''.join("file '"+str(root/x['file']).replace("'","'\\''")+"'\n" for x in segments))
    wav=root/'interview-60min.wav'
    subprocess.run(['ffmpeg','-v','error','-y','-f','concat','-safe','0','-i',str(root/'concat.txt'),'-t','3600','-ac','1','-ar','16000','-c:a','pcm_s16le',str(wav)],check=True)
    for minutes in [10,60]:
        seconds=minutes*60
        ref={'source':SOURCE,'license':'CC BY-NC 2.0; local evaluation only; not distributed with FilmCraft','speaker':a.speaker,'durationSeconds':seconds,'segments':segments,'words':[w for w in all_words if w['start']>=0 and w['end']<=seconds],'excluded':[dict(w,end=min(w['end'],seconds)) for w in excluded if w['start']<seconds],'silences':[dict(w,end=min(w['end'],seconds)) for w in silences if w['start']<seconds],'notes':'Consecutive segments from one human interview. No looping, TTS, padding or stretched speech. Interviewer/private/unknown/laughing-word labels excluded from scoring.'}
        (root/f'reference-{minutes}min.json').write_text(json.dumps(ref,ensure_ascii=False,indent=2))
        movie=root/f'real-korean-{minutes}min.mp4'
        subprocess.run(['ffmpeg','-v','error','-y','-f','lavfi','-i','color=c=0x24364a:s=320x180:r=24','-i',str(wav),'-t',str(seconds),'-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-c:a','aac','-b:a','64k',str(movie)],check=True)
        print(json.dumps({'minutes':minutes,'words':len(ref['words']),'excludedSeconds':sum(w['end']-w['start'] for w in ref['excluded']),'file':str(movie)},ensure_ascii=False),flush=True)
    (root/'provenance.json').write_text(json.dumps({'source':SOURCE,'license':'CC BY-NC 2.0','speaker':a.speaker,'originalSeconds':offset,'segments':segments,'video':'Original plain-color 320x180 24fps background carrying the unchanged chronology of real interview speech, resampled to mono16k and AAC-encoded. Not original camera footage.'},ensure_ascii=False,indent=2))
if __name__=='__main__':main()
