from pathlib import Path
import argparse,json,subprocess,tempfile,shutil
import numpy as np
p=argparse.ArgumentParser();p.add_argument('root',type=Path);p.add_argument('--minutes',type=int,required=True);p.add_argument('--phase',required=True);a=p.parse_args()
out=a.root/f'{a.phase}-{a.minutes}min';report=json.loads((out/'report.json').read_text());cs=json.loads((out/'workflow-candidates.json').read_text());selected=set(report['workflow']['selected']);tb=254016000000;rate=48000
cuts=[]
for c in sorted((c for c in cs['candidates'] if c['id'] in selected),key=lambda c:c['start']):
 s,e=c['start']/tb,c['end']/tb
 if cuts and s<=cuts[-1][1]:cuts[-1][1]=max(e,cuts[-1][1])
 else:cuts.append([s,e])
assert all(abs(t*rate-round(t*rate))<1e-3 for cut in cuts for t in cut), '48kHz must exactly cover the selected frame grid'
gold=json.loads((a.root/f'reference-{a.minutes}min.json').read_text())['words'];fail=[];results=[];quiet=0
with tempfile.TemporaryDirectory(prefix='audio-oracle48-',dir=out) as temp:
 def decode(path,name):
  dest=Path(temp)/name;subprocess.run(['ffmpeg','-v','error','-i',str(path),'-vn','-ac','1','-ar',str(rate),'-f','f32le',str(dest)],check=True)
  return np.memmap(dest,dtype='<f4',mode='r')
 x=decode(a.root/f'real-korean-{a.minutes}min.mp4','source.f32');y=decode(out/'edited.mp4','export.f32')
 for i,w in enumerate(gold):
  if w['text'].strip(' ,.?!') in {'음','어','으음','어어'}:continue
  s,e=w['start'],w['end'];assert all(min(e,b)<=max(s,a)+1e-6 for a,b in cuts),(i,'ordinary word cut')
  if e-s<.06:continue
  removed=sum(b-a for a,b in cuts if b<=s+1e-6);removed_samples=round(removed*rate);assert abs(removed*rate-removed_samples)<1e-3
  output=round((s-removed)*rate);start=output+removed_samples;n=min(round((e-s)*rate),len(x)-start,len(y)-output);assert n>0
  source=x[start:start+n].astype(np.float64);rms=float(np.sqrt(np.mean(source**2)))
  if rms<.001:quiet+=1;continue
  source-=source.mean();norm=float(np.linalg.norm(source));scores=[]
  for lag in range(-3,4):
   if output+lag<0:continue
   dest=y[output+lag:output+lag+n].astype(np.float64)
   if len(dest)!=n:continue
   dest-=dest.mean();den=norm*float(np.linalg.norm(dest));scores.append((float(np.dot(source,dest)/den) if den>0 else 0,lag))
  score,lag=max(scores,default=(0,0));results.append({'wordIndex':i,'sourceStart':s,'seconds':e-s,'correlation':score,'lagSamples':lag})
  if score<.9:fail.append(results[-1])
 del x,y
summary={'method':'Every human-annotated non-filler word >=60ms above -60dBFS; independent ffmpeg source/export decoding at 48kHz, where all selected 24fps cuts have integer sample boundaries. Exact selected-cut mapping; mean-centered waveform correlation >=0.9; lag search +/-3 samples (same +/-62.5us limit as +/-1 sample at 16kHz). No fractional interpolation. Lexical labels do not determine product behavior.','sampleRate':rate,'maximumLagSeconds':3/rate,'checkedWords':len(results),'quietWordsSkipped':quiet,'ordinaryOverlapSeconds':0,'minimumCorrelation':min((x['correlation'] for x in results),default=None),'failedWords':len(fail),'worst':sorted(results,key=lambda x:x['correlation'])[:10],'passed':bool(results) and not fail}
prev=out/'ordinary-word-audio-check.json';backup=out/'ordinary-word-audio-check-linear16k.json'
if prev.exists() and not backup.exists():shutil.copyfile(prev,backup)
prev.write_text(json.dumps(summary,indent=2));print(json.dumps({k:v for k,v in summary.items() if k not in ['method','worst']}));assert summary['passed'],fail[:10]
