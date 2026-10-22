#!/usr/bin/env python3
"""Score human TextGrid references against FilmCraft output; no ASR output is used as gold."""
import argparse,difflib,json,math,re,unicodedata
from pathlib import Path
TICKS=254016000000
FILLERS={'음','어','으음','어어'}
def norm(s):
    return ''.join(c for c in unicodedata.normalize('NFKC',s).casefold() if c.isalnum())
def distance(a,b):
    """Exact unit-cost Levenshtein distance, Myers bit vectors using Python integers."""
    if not a:return len(b)
    masks={}
    for i,c in enumerate(a):masks[c]=masks.get(c,0)|(1<<i)
    positive,negative,score,top=~0,0,len(a),1<<(len(a)-1)
    for c in b:
        eq=masks.get(c,0)
        d=(((eq&positive)+positive)^positive)|eq|negative
        hp=negative|~(d|positive);hn=positive&d
        if hp&top:score+=1
        elif hn&top:score-=1
        hp=(hp<<1)|1
        positive=(hn<<1)|~(d|hp);negative=d&hp
    return score
def percentile(values,q):
    if not values:return None
    s=sorted(values);return s[round((len(s)-1)*q)]
def overlap(a,b):return max(0,min(a['end'],b['end'])-max(a['start'],b['start']))
def evaluate(reference,hypothesis,candidates=None):
    masks=reference.get('excluded',[])
    tb=hypothesis.get('timebase',TICKS)
    raw=[dict(w,start=w['start']/tb,end=w['end']/tb) for w in hypothesis['words']]
    ignored=lambda w:any(m['start']<=(w['start']+w['end'])/2<m['end'] for m in masks)
    gold=[w for w in reference['words'] if norm(w['text'])]
    hyp=[w for w in raw if norm(w['text']) and not ignored(w)]
    rt=[norm(w['text']) for w in gold];ht=[norm(w['text']) for w in hyp]
    rc=''.join(rt);hc=''.join(ht)
    wd=distance(rt,ht);cd=distance(rc,hc)
    # Text-only ordered exact blocks avoid selecting matches by their timing error.
    pairs=[]
    for block in difflib.SequenceMatcher(None,rt,ht,autojunk=False).get_matching_blocks():
        if block.size>=3:pairs.extend((block.a+i,block.b+i) for i in range(block.size))
    starts=[abs(gold[a]['start']-hyp[b]['start']) for a,b in pairs]
    ends=[abs(gold[a]['end']-hyp[b]['end']) for a,b in pairs]
    rows=[]
    for c in (candidates or {}).get('candidates',[]):
        if c['kind']!='filler':continue
        cut=dict(c,start=c['start']/TICKS,end=c['end']/TICKS)
        if ignored(cut):continue
        positive=[w for w in gold if norm(w['text']) in FILLERS and overlap(w,cut)>0]
        intrusion=sum(overlap(w,cut) for w in gold if norm(w['text']) not in FILLERS)
        rows.append({'id':c['id'],'actionable':c.get('actionable',True),'text':c['text'],'start':cut['start'],'end':cut['end'],'lexicalHit':bool(positive),'nonFillerOverlapSeconds':intrusion,'referenceContext': ' '.join(w['text'] for w in gold if cut['start']-2<=w['start']<=cut['end']+2)})
    hits=sum(r['lexicalHit'] for r in rows);unsafe=sum(r['nonFillerOverlapSeconds']>.03 for r in rows)
    matched_gold={i for i,w in enumerate(gold) if norm(w['text']) in FILLERS and any(overlap(w,dict(start=r['start'],end=r['end']))>0 and r['lexicalHit'] for r in rows)}
    target_count=sum(norm(w['text']) in FILLERS for w in gold)
    active=[r for r in rows if r['actionable']]
    active_hits=sum(r['lexicalHit'] for r in active)
    active_gold={i for i,w in enumerate(gold) if norm(w['text']) in FILLERS and any(overlap(w,r)>0 for r in active)}
    active_metrics={'candidates':len(active),'blockedCandidates':len(rows)-len(active),'lexicalHits':active_hits,'falsePositives':len(active)-active_hits,'lexicalPrecision':active_hits/len(active) if active else None,'lexicalRecall':len(active_gold)/target_count if target_count else None,'cutsTouchingNonFillerOver30ms':sum(r['nonFillerOverlapSeconds']>.03 for r in active)}
    return {'referenceWords':len(rt),'hypothesisWords':len(ht),'referenceCharacters':len(rc),'wordEditDistance':wd,'characterEditDistance':cd,'wer':wd/len(rt) if rt else None,'cer':cd/len(rc) if rc else None,
        'excludedSeconds':sum(m['end']-m['start'] for m in masks),'ignoredHypothesisWords':len(raw)-len(hyp),
        'boundaries':{'method':'text-only exact ordered blocks of at least three words; conditional on correct words','matchedWords':len(pairs),'referenceCoverage':len(pairs)/len(gold) if gold else None,'startMedianSeconds':percentile(starts,.5),'startP95Seconds':percentile(starts,.95),'endMedianSeconds':percentile(ends,.5),'endP95Seconds':percentile(ends,.95),'signedStartMedianSeconds':percentile([hyp[b]['start']-gold[a]['start'] for a,b in pairs],.5),'signedEndMedianSeconds':percentile([hyp[b]['end']-gold[a]['end'] for a,b in pairs],.5),'over500ms':sum(a>.5 or b>.5 for a,b in zip(starts,ends))},
        'fillers':{'scope':'lexical target-word detection and cut overlap; not semantic filler classification','actionable':active_metrics,'referenceTargetWords':target_count,'candidates':len(rows),'unscorableCandidates':sum(c['kind']=='filler' for c in (candidates or {}).get('candidates',[]))-len(rows),'lexicalHits':hits,'falsePositives':len(rows)-hits,'lexicalPrecision':hits/len(rows) if rows else None,'lexicalRecall':len(matched_gold)/target_count if target_count else None,'cutsTouchingNonFillerOver30ms':unsafe,'details':rows},
        'timingAnomalies':{'zeroDurationWords':sum(w['end']<=w['start'] for w in raw),'overlaps':sum(a['end']>b['start']+.001 for a,b in zip(raw,raw[1:])),'wordsOver2Seconds':[dict(text=w['text'],start=w['start'],end=w['end']) for w in raw if w['end']-w['start']>2]}}
def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('reference',type=Path);p.add_argument('hypothesis',type=Path);p.add_argument('--candidates',type=Path);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    report=evaluate(json.loads(a.reference.read_text()),json.loads(a.hypothesis.read_text()),json.loads(a.candidates.read_text()) if a.candidates else None)
    a.output.write_text(json.dumps(report,ensure_ascii=False,indent=2))
    print(json.dumps({k:v for k,v in report.items() if k!='fillers'},ensure_ascii=False))
if __name__=='__main__':main()
