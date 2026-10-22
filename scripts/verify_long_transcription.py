#!/usr/bin/env python3
"""Exercise real desktop UI thread, Whisper-small, cancellation/retry, project round trips and exports."""
from pathlib import Path
import argparse,array,hashlib,json,math,os,socket,subprocess,time,statistics
ROOT=Path(__file__).resolve().parents[1];OUT=ROOT/'target/verification-p1';OUT.mkdir(parents=True,exist_ok=True)
TICKS=254016000000
class Desktop:
 def __init__(self):
  self.latencies=[];self.beats=0
  with socket.socket() as probe:
   probe.bind(('127.0.0.1',0));self.port=probe.getsockname()[1]
  self.log=(OUT/'desktop.log').open('w')
  data=OUT/'data';data.mkdir(exist_ok=True)
  if not (data/'models').exists(): (data/'models').symlink_to(Path.home()/'Library/Application Support/FilmCraft/models')
  self.binary=Path(os.environ.get('FILMCRAFT_QA_APP_BINARY',str(ROOT/'target/debug/filmcraft')))
  self.p=subprocess.Popen([str(self.binary),'--empty','--no-recover','--control',str(self.port),'--control-input-only','--data-dir',str(data)],stdout=self.log,stderr=self.log)
  if Path('/usr/sbin/taskpolicy').exists():subprocess.run(['/usr/sbin/taskpolicy','-B','-p',str(self.p.pid)],check=True)
  self.keep_awake = subprocess.Popen(['/usr/bin/caffeinate','-i','-d','-w',str(self.p.pid)]) if os.environ.get('FILMCRAFT_QA_KEEP_AWAKE')=='1' and Path('/usr/bin/caffeinate').exists() else None
  deadline=time.monotonic()+60
  while True:
   try: self.sock=socket.create_connection(('127.0.0.1',self.port),timeout=5);break
   except OSError:
    assert self.p.poll() is None,'desktop exited'
    assert time.monotonic()<deadline,'desktop did not start'
    time.sleep(.5)
  self.sock.settimeout(15);self.file=self.sock.makefile('rwb')
 def call(self,method,params=None,measure=False):
  start=time.monotonic();self.file.write((json.dumps({'method':method,'params':params or {}})+'\n').encode());self.file.flush()
  try:line=self.file.readline()
  except (TimeoutError,OSError) as error:
   if (OUT/'control-timeout.json').exists():raise
   diagnostic={'method':method,'params':params or {},'error':str(error),'processRunning':self.p.poll() is None}
   if Path('/usr/bin/sample').exists() and self.p.poll() is None:
    try:subprocess.run(['/usr/bin/sample',str(self.p.pid),'2','-file',str(OUT/'timeout-sample.txt')],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=10)
    except (OSError,subprocess.TimeoutExpired) as probe_error:diagnostic['sampleError']=str(probe_error)
   try:
    with socket.create_connection(('127.0.0.1',self.port),timeout=2) as probe:
     probe.settimeout(2);probe.sendall(b'{"method":"ui.inspect","params":{}}\n');diagnostic['independentUiInspect']=json.loads(probe.makefile('rb').readline())
   except (OSError,ValueError) as probe_error:diagnostic['independentUiInspectError']=str(probe_error)
   (OUT/'control-timeout.json').write_text(json.dumps(diagnostic,ensure_ascii=False,indent=2))
   raise
  assert line,'control connection closed'
  value=json.loads(line);assert value.get('ok'),(method,params,value)
  elapsed=time.monotonic()-start
  if measure:self.latencies.append(elapsed)
  return value.get('result')
 def cmd(self,command,params=None):return self.call('engine.execute',{'command':command,'params':params or {}})
 def heartbeat(self):
  state=self.call('ui.inspect',measure=True)
  self.call('ui.set',{'timeline':{'scroll':0}},measure=True)
  self.beats+=1
  # An occluded viewport intentionally runs logic without widgets. Its read/write
  # responsiveness is still measured; exercise real tabs when widgets are shown.
  if self.beats%20==0 and state.get('input',{}).get('viewportVisible') is not False:
   self.call('ui.click',{'id':'text.tab.Captions'},measure=True)
   self.call('ui.click',{'id':'text.tab.Transcript'},measure=True)
 def job(self,ident):return next(j for j in self.cmd('transcript.jobs') if j['id']==ident)
 def wait(self,ident,timeout=5400):
  started=time.monotonic();end=started+timeout;last=started;progress=[]
  while True:
   self.heartbeat();j=self.job(ident);progress.append(j['progress'])
   if j['finished']:break
   assert time.monotonic()<end,'transcription timed out'
   if time.monotonic()-last>30:
    print(json.dumps({'job':ident,'elapsedSeconds':time.monotonic()-started,'progress':j['progress'],'status':j['status']},ensure_ascii=False),flush=True);last=time.monotonic()
   time.sleep(.5)
  assert all(b>=a for a,b in zip(progress,progress[1:])),'progress moved backwards'
  return j
 def close(self):
  try:self.call('app.quit')
  except (OSError,AssertionError):pass
  try:self.p.wait(timeout=15)
  except subprocess.TimeoutExpired:self.p.terminate();self.p.wait(timeout=15)
  if self.keep_awake is not None:
   try:self.keep_awake.wait(timeout=5)
   except subprocess.TimeoutExpired:self.keep_awake.terminate();self.keep_awake.wait(timeout=5)
  self.log.close()
def project(path):return json.loads(path.read_text())['project']
def digest(path):return hashlib.file_digest(path.open('rb'),'sha256').hexdigest()
def audio_rms(path,offset):
 raw=subprocess.check_output(['ffmpeg','-v','error','-ss',str(offset),'-i',str(path),'-t','13','-vn','-ac','1','-ar','16000','-f','f32le','-'])
 samples=array.array('f',raw);assert samples,'no exported audio samples'
 return math.sqrt(sum(x*x for x in samples)/len(samples))
def main():
 parser=argparse.ArgumentParser(description=__doc__);parser.add_argument("--minutes",type=int,nargs="+",choices=[10,60],default=[10,60]);args=parser.parse_args()
 report_path=OUT/"report.json"
 reports=[r for r in (json.loads(report_path.read_text()) if report_path.exists() else []) if r["minutes"] not in args.minutes]
 d=Desktop()
 try:
  d.call('ui.set',{'workspace':'Captions and Graphics'})
  for minutes in args.minutes:
   folder=OUT/f'{minutes}min';folder.mkdir(exist_ok=True);media=OUT/f'korean-{minutes}min.mp4';sha=digest(media)
   d.cmd('file.newProject',{'name':f'P1 Korean {minutes} minutes'})
   item=d.cmd('file.import',{'paths':[str(media)]})['items'][0]
   d.cmd('file.newSequenceFromClip',{'items':[item]})
   d.call('ui.click',{'id':'text.tab.Transcript'})
   before=folder/'before.fcproj';d.cmd('file.saveAs',{'path':str(before)});original=project(before)
   start=time.monotonic();r=d.cmd('transcript.generate',{'items':[item],'language':'ko','model':'whisper-small','diarize':False});start_latency=time.monotonic()-start
   jid=r['job'];deadline=time.monotonic()+300
   # Cancel during actual speech processing, after model loading and audio decoding.
   while True:
    d.heartbeat();j=d.job(jid)
    assert not j['finished'],j
    if 'Decoding speech' in j['status']:break
    assert time.monotonic()<deadline,j
    time.sleep(.2)
   elements=d.call('ui.elements',{'prefix':'text.transcription.'})
   assert any(e['id']=='text.transcription.progress' for e in elements),elements
   start=time.monotonic();d.call('ui.click',{'id':'text.transcription.cancel'});cancel_ack=time.monotonic()-start
   j=d.wait(jid,120);cancel_seconds=time.monotonic()-start
   assert j['canRetry'] and j['result'].get('error'),j
   cancelled=folder/'cancelled.fcproj';d.cmd('file.saveCopy',{'path':str(cancelled)})
   assert project(cancelled)==original,'cancel changed the project'
   assert digest(media)==sha,'cancel changed the media'
   d.call('ui.screenshot',{'path':str(folder/'cancelled.png')})
   # Use the visible Retry control rather than a new Transcribe request.
   d.call('ui.click',{'id':'text.transcription.retry'})
   jobs=d.cmd('transcript.jobs');retry=jobs[-1]['id'];assert retry!=jid
   begin=time.monotonic();j=d.wait(retry);elapsed=time.monotonic()-begin
   assert not j['result'].get('error'),j
   tr=d.cmd('transcript.inspect');words=tr['words'];spoken=' '.join(w['text'] for w in words)
   assert sum('\uac00'<=c<='\ud7a3' for c in spoken)>10,spoken
   assert max(w['start'] for w in words)>TICKS*(minutes*60-(60 if minutes==10 else 300)-15),'late speech missing'
   transcript=folder/'transcribed.fcproj';d.cmd('file.saveAs',{'path':str(transcript)});completed=project(transcript)
   d.cmd('edit.undo');undo=folder/'undo.fcproj';d.cmd('file.saveCopy',{'path':str(undo)});assert project(undo)==original,'Undo did not restore full project'
   d.cmd('edit.redo');redo=folder/'redo.fcproj';d.cmd('file.saveCopy',{'path':str(redo)});assert project(redo)==completed,'Redo changed result'
   d.cmd('file.open',{'path':str(transcript)});assert d.cmd('transcript.inspect')['words']==words,'save/reopen changed transcript'
   d.call('ui.screenshot',{'path':str(folder/'complete.png')})
   # Export runs in the existing export worker; keep sending actual UI requests meanwhile.
   movie=folder/f'korean-{minutes}min-export.mp4'
   export=d.cmd('file.exportMedia',{'path':str(movie),'format':'h264','audio':True,'wait':False})
   ejob=export['job'];end=time.monotonic()+5400
   while True:
    d.heartbeat();ej=next(j for j in d.cmd('jobs.list') if j['id']==ejob)
    if ej['finished']:break
    assert time.monotonic()<end,'export timeout';time.sleep(1)
   assert not ej['result'].get('error'),ej
   probe=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-show_format','-of','json',str(movie)]))
   video=next(s for s in probe['streams'] if s['codec_type']=='video')
   assert int(video['nb_frames'])==minutes*60,video
   assert abs(float(video['duration'])-minutes*60)<.01,video
   subprocess.run(['ffmpeg','-v','error','-i',str(movie),'-f','null','-'],check=True)
   audio=[]
   for offset in [0,minutes*60-(60 if minutes==10 else 300)]:
    source_rms=audio_rms(media,offset);export_rms=audio_rms(movie,offset)
    assert source_rms>.001 and source_rms*.5<export_rms<source_rms*1.5,(offset,source_rms,export_rms)
    audio.append({'offsetSeconds':offset,'sourceRMS':source_rms,'exportRMS':export_rms})
   assert digest(media)==sha
   latency=d.latencies;d.latencies=[]
   report={'minutes':minutes,'fixture':'synthetic Korean Yuna speech every '+str(60 if minutes==10 else 300)+' seconds; 320x180 1fps','model':'whisper-small','startSeconds':start_latency,'cancelUIAckSeconds':cancel_ack,'cancelWorkerSeconds':cancel_seconds,'transcribeSeconds':elapsed,'words':len(words),'lastWordSeconds':max(w['start'] for w in words)/TICKS,'uiRequests':len(latency),'uiMedianSeconds':statistics.median(latency),'uiP95Seconds':sorted(latency)[int((len(latency)-1)*.95)],'uiMaxSeconds':max(latency),'cancelProjectUnchanged':True,'sourceUnchanged':True,'sourceSHA256':sha,'undoRedo':True,'saveReopen':True,'exportSeconds':float(video['duration']),'exportFrames':int(video['nb_frames']),'exportFullDecode':True,'exportAudio':audio}
   reports.append(report);(OUT/'report.json').write_text(json.dumps(reports,ensure_ascii=False,indent=2));print(json.dumps(report,ensure_ascii=False),flush=True)
 finally:d.close()
if __name__=='__main__':main()
