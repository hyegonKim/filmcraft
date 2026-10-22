#!/usr/bin/env python3
"""Independent source/export audio oracle; licensed media stays outside the repository."""
import argparse,array,json,math,subprocess
from pathlib import Path
T=254016000000

def samples(path,at):
 raw=subprocess.check_output(['ffmpeg','-v','error','-ss',f'{at:.9f}','-i',str(path),'-t','3','-vn','-ac','1','-ar','16000','-f','f32le','-']);v=array.array('f',raw);assert len(v)>40000
 return v

def corr(a,b,lag=0,step=1):
 if lag>=0:x=a[lag::step];y=b[:len(a)-lag:step]
 else:x=a[:len(a)+lag:step];y=b[-lag::step]
 n=min(len(x),len(y));x=x[:n];y=y[:n];sx=sum(x);sy=sum(y);xx=sum(t*t for t in x)-sx*sx/n;yy=sum(t*t for t in y)-sy*sy/n
 return (sum(p*q for p,q in zip(x,y))-sx*sy/n)/math.sqrt(xx*yy) if xx>0 and yy>0 else 0

def main():
 p=argparse.ArgumentParser();p.add_argument('root',type=Path);p.add_argument('--minutes',type=int,choices=[10,60],required=True);p.add_argument('--phase',default='corrected');a=p.parse_args();folder=a.root/f'{a.phase}-{a.minutes}min';r=json.loads((folder/'report.json').read_text());w=r['workflow'];cs=json.loads((folder/'workflow-candidates.json').read_text())['candidates'];cuts=sorted((c['start']/T,c['end']/T) for c in cs if c['id'] in w['selected']);merged=[]
 for s,e in cuts:
  if merged and s<=merged[-1][1]:merged[-1][1]=max(e,merged[-1][1])
  else:merged.append([s,e])
 removed=0;positions=[]
 for s,e in merged:positions.append(s-removed);removed+=e-s
 assert abs(a.minutes*60-removed-w['exportSeconds'])<1/24+.001
 results=[]
 for at in [1,w['exportSeconds']*.25,w['exportSeconds']*.5,w['exportSeconds']*.75,w['exportSeconds']-15]:
  for pos in positions:
   if at-.1<=pos<at+3.1:at=pos+.2
  source_at=at
  for s,e in merged:
   if source_at>=s:source_at+=e-s
  x=samples(a.root/f'real-korean-{a.minutes}min.mp4',source_at);y=samples(folder/'edited.mp4',at);score=corr(x,y);lag=0
  if score<.90:
   coarse=max(range(-1600,1601,16),key=lambda k:corr(x,y,k,16));lag=max(range(coarse-16,coarse+17),key=lambda k:corr(x,y,k));score=corr(x,y,lag)
  result={'outputSeconds':at,'sourceSeconds':source_at,'normalizedCorrelation':score,'lagSeconds':lag/16000};results.append(result);print(json.dumps(result),flush=True)
  assert score>=.90 and abs(lag/16000)<=.04,result
 (folder/'audio-content-check.json').write_text(json.dumps({'method':'Independent ffmpeg decode of five source/output windows, mapping source positions from merged selected cuts; normalized waveform correlation; 40ms maximum allowed lag.','passed':True,'windows':results},indent=2))
if __name__=='__main__':main()
