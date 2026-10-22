#!/usr/bin/env python3
"""Original synthetic Korean QA clips: speech every minute / five minutes, long digital silence."""
from pathlib import Path
import array, subprocess, wave, json
root=Path(__file__).resolve().parents[1]
out=root/'target/verification-p1';out.mkdir(parents=True,exist_ok=True)
raw=subprocess.check_output(['ffmpeg','-v','error','-i',str(root/'target/fixtures/korean-speech.wav'),'-ar','16000','-ac','1','-f','s16le','-'])
for minutes,period in [(10,60),(60,300)]:
 path=out/f'korean-{minutes}min.wav'
 with wave.open(str(path),'wb') as w:
  w.setparams((1,2,16000,0,'NONE','not compressed'))
  for offset in range(0,minutes*60,period):
   w.writeframesraw(raw)
   w.writeframesraw(bytes(period*16000*2-len(raw)))
 movie=out/f'korean-{minutes}min.mp4'
 subprocess.run(['ffmpeg','-v','error','-y','-f','lavfi','-i','color=c=0x24364a:s=320x180:r=1','-i',str(path),'-t',str(minutes*60),'-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-c:a','aac','-b:a','48k',str(movie)],check=True)
 print(json.dumps({'file':str(movie),'seconds':minutes*60,'speechEverySeconds':period,'kind':'synthetic Yuna Korean speech and silence','video':'320x180 1fps H264 AAC'}),flush=True)
