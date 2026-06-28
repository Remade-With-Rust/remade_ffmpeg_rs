import sys, csv, numpy as np
from scipy.stats import pearsonr, spearmanr
rows=[r for r in csv.DictReader(open(sys.argv[1]))]
def f(r,k):
    try: return float(r[k])
    except: return None
rows=[r for r in rows if f(r,'odg') is not None and f(r,'nmr_mean') is not None]
odg=np.array([f(r,'odg') for r in rows]); nmr=np.array([f(r,'nmr_mean') for r in rows])
nmx=np.array([f(r,'nmr_max') for r in rows]); pct=np.array([f(r,'pct') for r in rows])
enc=np.array([r['enc'] for r in rows])
print('N =',len(rows))
print('\n=== Does our NMR predict the perceptual ODG? (lower NMR should mean higher ODG) ===')
for name,x in [('mean NMR',nmr),('max NMR',nmx),('%% audible',pct)]:
    pr,_=pearsonr(x,odg); sr,_=spearmanr(x,odg)
    print('  %-10s vs ODG:  Pearson %+.2f  Spearman %+.2f'%(name,pr,sr))
print('  (a GOOD self-metric has STRONG NEGATIVE corr: lower NMR -> higher ODG)')
for e in ['ours','lame']:
    m=enc==e
    if m.sum()>2:
        sr,_=spearmanr(nmr[m],odg[m]); print('  within %-4s: mean NMR vs ODG Spearman %+.2f'%(e,sr))
print('\n=== The UNBIASED verdict: ours vs LAME by PEAQ ODG (higher = better) ===')
clips=sorted(set(r['clip'] for r in rows)); brs=sorted(set(int(r['br']) for r in rows))
ours_wins=lame_wins=0; nmr_says_ours=0
print('  %-8s %-5s %8s %8s   %-12s | NMR says'%('clip','br','ours ODG','lame ODG','PEAQ winner'))
for c in clips:
    for b in brs:
        o=[r for r in rows if r['clip']==c and int(r['br'])==b and r['enc']=='ours']
        l=[r for r in rows if r['clip']==c and int(r['br'])==b and r['enc']=='lame']
        if not o or not l: continue
        oo,ll=f(o[0],'odg'),f(l[0],'odg'); on,ln=f(o[0],'nmr_mean'),f(l[0],'nmr_mean')
        win='ours' if oo>ll else 'lame'; 
        if oo>ll: ours_wins+=1
        else: lame_wins+=1
        nsays='ours' if on<ln else 'lame'
        if nsays=='ours': nmr_says_ours+=1
        flag='' if win==nsays else '  <-- NMR WRONG'
        print('  %-8s %-5d %+8.2f %+8.2f   %-12s | %s%s'%(c,b,oo,ll,win,nsays,flag))
print('\n  PEAQ verdict: ours wins %d, LAME wins %d (of %d)'%(ours_wins,lame_wins,ours_wins+lame_wins))
print('  Our NMR claims ours wins %d/%d -> the bias, now quantified.'%(nmr_says_ours,ours_wins+lame_wins))
