#!/usr/bin/env python3
"""Real interview ASR evaluation and selected cleanup / undo / save / export through the desktop."""
import argparse,hashlib,json,socket,statistics,subprocess,time
from pathlib import Path
import verify_long_transcription as desktop
from evaluate_korean_asr import evaluate,TICKS
from verify_ui_liveness import UIProbe

def project(path):return json.loads(path.read_text())['project']
def show_for_widget_checks(d):
    started=time.monotonic();d.call('ui.focus',measure=True);deadline=started+15
    while True:
        state=d.call('ui.inspect',measure=True)
        if state.get('elements',0)>0 and state['input']['viewportVisible'] and state['input']['secondsSinceUiPass']<.25:return time.monotonic()-started
        assert time.monotonic()<deadline, 'test window did not become visible for widget verification'
        time.sleep(.1)
def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('data',type=Path);p.add_argument('--minutes',type=int,choices=[1,10,60],required=True);p.add_argument('--phase',default='baseline');p.add_argument('--workflow',action='store_true');p.add_argument('--require-safe-fillers',action='store_true');p.add_argument('--ui-stress',action='store_true');p.add_argument('--preserve-fillers',action='store_true');p.add_argument('--reuse-project',type=Path);p.add_argument('--concurrent-export',action='store_true',help='export the original sequence during fresh transcription');a=p.parse_args()
    assert not (a.concurrent_export and a.reuse_project), 'concurrency validation requires fresh transcription'
    root=a.data.resolve();out=root/f'{a.phase}-{a.minutes}min';out.mkdir(exist_ok=True)
    media=root/f'real-korean-{a.minutes}min.mp4';reference=json.loads((root/f'reference-{a.minutes}min.json').read_text());sha=hashlib.file_digest(media.open('rb'),'sha256').hexdigest()
    desktop.OUT=out;d=desktop.Desktop();session_started=time.monotonic();phase=['setup'];concurrent_running=[False];events=(out/'ui-latency.jsonl').open('w');call=d.call
    def timed(method,params=None,measure=False):
        start=time.monotonic();r=call(method,params,measure)
        if measure:
            row={'phase':phase[0],'concurrentExportActive':concurrent_running[0],'method':method,'sinceStartSeconds':time.monotonic()-session_started,'seconds':time.monotonic()-start}
            if method=='ui.inspect':row['window']={k:r.get('input',{}).get(k) for k in ['viewportVisible','viewportOccluded','nativeWindowFocused','secondsSinceUiPass']}
            events.write(json.dumps(row)+'\n');events.flush()
        return r
    d.call=timed
    foreground={};liveness_probe=UIProbe(d.port,out/'ui-independent.jsonl',phase=lambda:phase[0],anchor=session_started) if a.ui_stress else None
    try:
        d.call('ui.set',{'workspace':'Captions and Graphics'})
        if a.reuse_project:d.cmd('file.open',{'path':str(a.reuse_project.resolve())})
        else:
            d.cmd('file.newProject',{'name':f'Real Korean {a.minutes}min QA'})
            imp=d.cmd('file.import',{'paths':[str(media)]});assert not imp.get('errors'),imp
            item=imp['items'][0];d.cmd('file.newSequenceFromClip',{'items':[item]})
        # Concurrent interactive QA explicitly presents its own test window. Pure queries
        # keep their background semantics; foreground restoration is reported separately.
        if a.ui_stress:foreground['setupSeconds']=show_for_widget_checks(d)
        d.call('ui.click',{'id':'text.tab.Transcript'})
        before=out/'before.fcproj';d.cmd('file.saveAs',{'path':str(before)})
        original=project(before);concurrent=None;concurrent_job=None;concurrent_checked=[0.0];concurrent_states=[]
        if a.concurrent_export:
            original_sequence=d.cmd('sequence.inspect')
            concurrent_movie=out/'concurrent-original.mp4'
            concurrent_job=d.cmd('file.exportMedia',{'path':str(concurrent_movie),'format':'h264','audio':True,'wait':False})['job']
            concurrent_running[0]=True
            heartbeat=d.heartbeat
            def concurrent_heartbeat():
                heartbeat()
                if time.monotonic()-concurrent_checked[0]>=2:
                    concurrent_checked[0]=time.monotonic()
                    state=next(j for j in d.cmd('jobs.list') if j['id']==concurrent_job)
                    concurrent_running[0]=not state['finished']
                    concurrent_states.append({'sinceStartSeconds':time.monotonic()-session_started,'finished':state['finished'],'progress':state['progress']})
            d.heartbeat=concurrent_heartbeat
        phase[0]='transcription';started=time.monotonic()
        if not a.reuse_project:
            r=d.cmd('transcript.generate',{'items':[item],'language':'ko','model':'whisper-small','diarize':False,'preserveFillers':a.preserve_fillers})
            # The 60-minute fresh pass includes per-word rechecks. Under the observed
            # memory pressure it can outlast eight hours; keep observing UI throughout.
            job=d.wait(r['job'],timeout=(24 if a.minutes==60 else 8)*3600);assert not job['result'].get('error'),job
        seconds=time.monotonic()-started
        if concurrent_job is not None:
            d.heartbeat=heartbeat
            assert any(not state['finished'] for state in concurrent_states), 'no measured export/transcription overlap'
            deadline=time.monotonic()+8*3600
            while True:
                d.heartbeat();state=next(j for j in d.cmd('jobs.list') if j['id']==concurrent_job)
                if state['finished']:break
                assert time.monotonic()<deadline, 'concurrent export timeout'
                time.sleep(.5)
            concurrent_running[0]=False
            assert not state['result'].get('error'), state
            probe=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-of','json',str(concurrent_movie)]))
            video=next(s for s in probe['streams'] if s['codec_type']=='video')
            assert int(video['nb_frames'])==original_sequence['durationFrames']
            assert abs(float(video['duration'])-original_sequence['duration']/TICKS)<1/24+.001
            assert any(s['codec_type']=='audio' for s in probe['streams'])
            subprocess.run(['ffmpeg','-v','error','-i',str(concurrent_movie),'-f','null','-'],check=True)
            assert hashlib.file_digest(media.open('rb'),'sha256').hexdigest()==sha
            concurrent={'path':str(concurrent_movie),'job':concurrent_job,'overlapSamples':sum(not state['finished'] for state in concurrent_states),'fullDecode':True,'sourceUnchanged':True,'frames':int(video['nb_frames']),'seconds':float(video['duration'])}
            (out/'concurrent-export.json').write_text(json.dumps({'result':concurrent,'samples':concurrent_states},indent=2))
        phase[0]='review'
        if a.ui_stress:foreground['reviewSeconds']=show_for_widget_checks(d)
        hyp=d.cmd('transcript.inspect');hyp['timebase']=TICKS
        (out/'transcript.json').write_text(json.dumps(hyp,ensure_ascii=False,indent=2))
        saved=out/'transcribed.fcproj';d.cmd('file.saveAs',{'path':str(saved)})
        transcription_undo=None
        if not a.reuse_project:
            completed=project(saved)
            d.cmd('edit.undo');undone=out/'transcription-undo.fcproj';d.cmd('file.saveCopy',{'path':str(undone)})
            assert project(undone)==original, 'transcription Undo did not restore full project'
            d.cmd('edit.redo');redone=out/'transcription-redo.fcproj';d.cmd('file.saveCopy',{'path':str(redone)})
            assert project(redone)==completed, 'transcription Redo changed project'
            transcription_undo=True
        preview=d.cmd('transcript.previewCleanup',{'minSeconds':.8,'keepSeconds':.15,'fillers':['음','어','으음','어어']})
        (out/'candidates.json').write_text(json.dumps(preview,ensure_ascii=False,indent=2))
        evaluation=evaluate(reference,hyp,preview);(out/'accuracy.json').write_text(json.dumps(evaluation,ensure_ascii=False,indent=2))
        if a.require_safe_fillers:
            approved=[x for x in evaluation['fillers']['details'] if x['actionable']]
            assert approved, 'no approved filler: recall improvement not established'
            assert all(x['lexicalHit'] and x['nonFillerOverlapSeconds']<=1e-6 for x in approved), 'approved filler invades ordinary speech or is a lexical false positive'

        (out/'transcription-timing.json').write_text(json.dumps({'seconds':seconds,'reusedProject':bool(a.reuse_project)},indent=2))
        # Exercise bounded native transcript layout and reach the final word of long projects.
        d.call('ui.click',{'id':'text.tab.Transcript'},measure=True)
        assert d.call('ui.inspect')['ui']['text_tab']=='Transcript', 'page check requires Transcript tab'
        rendered=d.call('ui.elements',{'prefix':'text.transcript.word.'},measure=True)
        assert rendered, 'Transcript tab has no rendered words after transcription'
        assert len(rendered)<=500, f'unbounded transcript layout: {len(rendered)}'
        pagination={'firstPageWords':len(rendered),'lastPageReachable':True}
        if len(hyp.get('words',[]))>500:
            d.call('ui.click',{'id':'text.transcript.page.last'},measure=True)
            last_page=d.call('ui.elements',{'prefix':'text.transcript.word.'},measure=True)
            assert len(last_page)<=500
            assert any(e['id']==f"text.transcript.word.{len(hyp['words'])-1}" for e in last_page), 'last transcript word inaccessible'
            pagination['lastPageWords']=len(last_page)
            panel=next(e['rect'] for e in d.call('ui.elements',{'prefix':'panel.Text'}) if e['id']=='panel.Text')
            d.call('ui.scroll',{'x':panel[0]+min(100,panel[2]/2),'y':panel[1]+panel[3]-50,'dx':0,'dy':-100000},measure=True)
            visible=False
            for _ in range(12):
                elements=d.call('ui.elements',{'prefix':f"text.transcript.word.{len(hyp['words'])-1}"},measure=True)
                last=next(e['rect'] for e in elements if e['id']==f"text.transcript.word.{len(hyp['words'])-1}")
                visible=panel[1]<=last[1] and last[1]+last[3]<=panel[1]+panel[3]
                if visible:break
            assert visible, 'last word registered but cannot scroll into the visible panel'
            pagination['lastWordVisible']=visible
        if a.ui_stress:
            phase[0]='ui-stress'
            for cycle in range(100):
                d.call('ui.click',{'id':'text.tab.Captions'},measure=True)
                d.call('ui.click',{'id':'text.tab.Transcript'},measure=True)
                state=d.call('ui.inspect',measure=True)
                assert state['ui']['text_tab']=='Transcript', 'input acknowledged before tab was processed'
                if len(hyp.get('words',[]))>500:
                    for button in ['previous','last']:
                        d.call('ui.click',{'id':f'text.transcript.page.{button}'},measure=True)
            phase[0]='review'
        # A capture can wait for a presented frame; keep its connection separate from UI liveness checks.
        screenshot_error=None;capture_started=time.monotonic()
        try:
            with socket.create_connection(('127.0.0.1',d.port),timeout=30) as capture:
                capture.settimeout(30)
                with capture.makefile('rwb') as stream:
                    stream.write((json.dumps({'method':'ui.screenshot','params':{'path':str(out/'transcribed.png')}})+'\n').encode());stream.flush()
                    result=json.loads(stream.readline())
                    if not result.get('ok'):screenshot_error=result.get('error','capture failed')
        except (OSError,ValueError) as error:screenshot_error=str(error)
        if screenshot_error:print(json.dumps({'screenshotUnavailable':screenshot_error}),flush=True)
        report={'foregroundWindow':foreground,'concurrentExport':concurrent,'minutes':a.minutes,'phase':a.phase,'model':'whisper-small','preserveFillers':a.preserve_fillers,'transcribeSeconds':seconds,'reusedProject':str(a.reuse_project) if a.reuse_project else None,'screenshotError':screenshot_error,'screenshotSeconds':time.monotonic()-capture_started,'accuracy':{k:v for k,v in evaluation.items() if k!='fillers'},'fillers':{k:v for k,v in evaluation['fillers'].items() if k!='details'},'workflow':None,'pagination':pagination,'transcriptionUndoRedo':transcription_undo,'binarySHA256':hashlib.sha256(d.binary.read_bytes()).hexdigest(),'binaryExecutable':str(d.binary)}
        (out/'report.json').write_text(json.dumps(report,ensure_ascii=False,indent=2))
        print(json.dumps(report,ensure_ascii=False),flush=True)
        if a.workflow:
            phase[0]='cleanup'
            # Oracle selection simulates reviewing against independent human annotations; it is not a runtime detector.
            safe=[r['id'] for r in evaluation['fillers']['details'] if r.get('actionable',True) and r['lexicalHit'] and r['nonFillerOverlapSeconds']<=1e-6 and not any(min(r['end'],m['end'])>max(r['start'],m['start']) for m in reference.get('excluded',[]))]
            selection_kind='filler'
            if not safe:
                selection_kind='pause'
                for params in [preview['params'],{'minSeconds':.2,'keepSeconds':.05,'fillers':['음','어','으음','어어']}]:
                    preview=d.cmd('transcript.previewCleanup',params)
                    safe=[c['id'] for c in preview['candidates'] if c['kind']=='pause' and any(s['start']<=c['start']/TICKS and s['end']>=c['end']/TICKS for s in reference.get('silences',[]))]
                    if safe:break
            assert safe,'no independently verified safe cleanup candidate; refuse to cut unverified speech'
            selected=safe if a.require_safe_fillers and selection_kind=='filler' else safe[:min(3,len(safe))]
            (out/'workflow-candidates.json').write_text(json.dumps(preview,ensure_ascii=False,indent=2))
            before_edit=project(saved);seq=d.cmd('sequence.inspect')
            result=d.cmd('transcript.applyCleanup',{'token':preview['token'],'params':preview['params'],'selected':selected})
            after=d.cmd('sequence.inspect');assert after['duration']==seq['duration']-result['ticks']
            edited=out/'edited.fcproj';d.cmd('file.saveAs',{'path':str(edited)});after_edit=project(edited)
            d.cmd('edit.undo');undo=out/'undo.fcproj';d.cmd('file.saveCopy',{'path':str(undo)});assert project(undo)==before_edit,'cleanup Undo changed unselected material'
            d.cmd('edit.redo');redo=out/'redo.fcproj';d.cmd('file.saveCopy',{'path':str(redo)});assert project(redo)==after_edit
            d.cmd('file.open',{'path':str(edited)});reopened=out/'reopened.fcproj';d.cmd('file.saveCopy',{'path':str(reopened)});assert project(reopened)==after_edit
            phase[0]='export';movie=out/'edited.mp4';job=d.cmd('file.exportMedia',{'path':str(movie),'format':'h264','audio':True,'wait':False});deadline=time.monotonic()+8*3600;last=time.monotonic()
            while True:
                d.heartbeat();j=next(j for j in d.cmd('jobs.list') if j['id']==job['job'])
                if j['finished']:break
                assert time.monotonic()<deadline,'export timed out'
                if time.monotonic()-last>30:print(json.dumps({'export':j['progress'],'status':j['status']}),flush=True);last=time.monotonic()
                time.sleep(.5)
            assert not j['result'].get('error'),j
            probe=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-show_format','-of','json',str(movie)]))
            video=next(s for s in probe['streams'] if s['codec_type']=='video');assert any(s['codec_type']=='audio' for s in probe['streams'])
            assert int(video['nb_frames'])==after['durationFrames'];assert abs(float(video['duration'])-after['duration']/TICKS)<1/24+.001
            subprocess.run(['ffmpeg','-v','error','-i',str(movie),'-f','null','-'],check=True)
            rms=desktop.audio_rms(movie,0);assert rms>.001
            late_rms=desktop.audio_rms(movie,max(0,float(video['duration'])-20));assert late_rms>.001
            assert hashlib.file_digest(media.open('rb'),'sha256').hexdigest()==sha
            report['workflow']={'selected':selected,'selectionKind':selection_kind,'oracleSelection':True,'undoRedo':True,'saveReopen':True,'sourceUnchanged':True,'sourceSHA256':sha,'exportFrames':int(video['nb_frames']),'exportSeconds':float(video['duration']),'exportAudioRMS':rms,'exportAudioLateRMS':late_rms,'exportFullDecode':True,'removedTicks':result['ticks']}
        rows=[json.loads(line) for line in (out/'ui-latency.jsonl').read_text().splitlines()]
        samples=[row['seconds'] for row in rows]
        report['ui']={'requests':len(samples),'p95Seconds':sorted(samples)[int((len(samples)-1)*.95)] if samples else None,'maxSeconds':max(samples) if samples else None}
        groups={name:[r['seconds'] for r in rows if r['phase']==name] for name in sorted({r['phase'] for r in rows})}
        groups['transcription+export']=[r['seconds'] for r in rows if r['phase']=='transcription' and r.get('concurrentExportActive')]
        report['ui']['byPhase']={name:{'requests':len(values),'p95Seconds':sorted(values)[int((len(values)-1)*.95)],'maxSeconds':max(values)} for name,values in groups.items() if values}
        inspected=[r for r in rows if r['method']=='ui.inspect']
        report['ui']['visibility']={'inspectSamples':len(inspected),'occludedSamples':sum(r.get('window',{}).get('viewportVisible') is False for r in inspected),'maximumSecondsWithoutUiPass':max((r.get('window',{}).get('secondsSinceUiPass') or 0 for r in inspected),default=0),'gesturePolicy':'periodic widget clicks only while visible; hidden logic inspect/set still timed; final100tab/pagecycles require successful widget input'}
        if liveness_probe:
            report['independentUI']=liveness_probe.stop()
            assert report['independentUI']['requests']>0 and report['independentUI']['errors']==0 and report['independentUI']['stopped'] and report['independentUI']['overSixSeconds']==0, report['independentUI']
        (out/'report.json').write_text(json.dumps(report,ensure_ascii=False,indent=2));print(json.dumps({'finished':str(out/'report.json'),'ui':report['ui'],'workflow':report['workflow']},ensure_ascii=False),flush=True)
    finally:
        if liveness_probe:(out/'ui-independent-summary.json').write_text(json.dumps(liveness_probe.stop(),indent=2))
        events.close();d.close()
if __name__=='__main__':main()
