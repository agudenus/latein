#!/usr/bin/env python3
"""
Test the revised hypothesis:
  Matchmaking pools == Century Games "Neighboring Kingdom Leaderboard groups".
  Those groups MERGE on a 28-day cadence, so group walls MOVE over time.
  => A kingdom's leak rate against CURRENT groups should scale with how many
     KvKs it has played (i.e. how many boundary epochs its history spans).
"""
import pandas as pd, numpy as np
from collections import defaultdict

# ---- Official alignment announcements (kingshotoptimizer bulletin) ----------
# (effective_date, [ranges merged], resulting group)
ALIGNMENTS = [
    ("2026-06-03", [(1,25),(26,115)],        (1,115)),
    ("2026-06-03", [(236,309),(310,417)],    (236,417)),
    ("2026-06-03", [(588,674),(675,758)],    (588,758)),
    ("2026-06-03", [(1087,1159),(1160,1221)],(1087,1221)),
    ("2026-07-29", [(759,846),(847,927)],    (759,927)),
    ("2026-07-29", [(1222,1277),(1278,1331)],(1222,1331)),
    ("2026-08-26", [(1332,1381),(1382,1426)],(1332,1426)),
    ("2026-09-23", [(1427,1502),(1503,1558)],(1427,1558)),
]
# Groups as they stood BEFORE each merge (the constituent blocks) and AFTER.
CURRENT_GROUPS = [g for _,_,g in ALIGNMENTS]
PRE_BLOCKS     = sorted({b for _,bs,_ in ALIGNMENTS for b in bs})

def group_of(k, groups):
    for lo,hi in groups:
        if lo<=k<=hi: return (lo,hi)
    return None

df = pd.read_csv('data/opponents_baseline.csv')
adj=defaultdict(set)
for _,r in df.iterrows():
    k=int(r.kingdom)
    for o in [int(x) for x in str(r.opponents).split()]:
        adj[k].add(o)
scraped=sorted(set(df.kingdom.astype(int)))

print("="*68)
print("TEST 1  Leak rate against CURRENT official groups, by cohort age")
print("="*68)
rows=[]
for k in scraped:
    g=group_of(k,CURRENT_GROUPS)
    if not g: continue
    opp=adj[k]
    inside=sum(1 for o in opp if g[0]<=o<=g[1])
    rows.append({'k':k,'group':f"{g[0]}-{g[1]}",'kvks':len(opp),
                 'inside':inside,'leak':len(opp)-inside,
                 'leak_pct':100*(len(opp)-inside)/len(opp)})
R=pd.DataFrame(rows)
summary=R.groupby('group').agg(kingdoms=('k','size'), mean_kvks=('kvks','mean'),
        total_matchups=('kvks','sum'), leaks=('leak','sum')).reset_index()
summary['leak_pct']=(100*summary.leaks/summary.total_matchups).round(1)
summary['mean_kvks']=summary.mean_kvks.round(1)
print(summary.to_string(index=False))

print("\n  Correlation between KvKs played and personal leak rate:")
c=np.corrcoef(R.kvks, R.leak_pct)[0,1]
print(f"    r = {c:+.3f}   (n={len(R)} kingdoms)")
print("    -> positive r means: the more cycles a kingdom has played, the more")
print("       of its history predates the current group boundaries.")

print("\n"+"="*68)
print("TEST 2  Same kingdoms, scored against PRE-MERGE constituent blocks")
print("="*68)
rows2=[]
for k in scraped:
    g=group_of(k,PRE_BLOCKS)
    if not g: continue
    opp=adj[k]
    inside=sum(1 for o in opp if g[0]<=o<=g[1])
    rows2.append({'k':k,'block':f"{g[0]}-{g[1]}",'kvks':len(opp),
                  'inside':inside,'leak':len(opp)-inside})
R2=pd.DataFrame(rows2)
s2=R2.groupby('block').agg(kingdoms=('k','size'),mean_kvks=('kvks','mean'),
       total=('kvks','sum'),leaks=('leak','sum')).reset_index()
s2['leak_pct']=(100*s2.leaks/s2.total).round(1)
s2['mean_kvks']=s2.mean_kvks.round(1)
print(s2.to_string(index=False))

print("\n"+"="*68)
print("TEST 3  K377 against its official group 236-417")
print("="*68)
me=377; g=(236,417)
opp=sorted(adj[me])
ins=[o for o in opp if g[0]<=o<=g[1]]
out=[o for o in opp if not (g[0]<=o<=g[1])]
print(f"  opponents        : {opp}")
print(f"  inside 236-417   : {ins}  ({len(ins)}/{len(opp)})")
print(f"  outside          : {out}")
print(f"  -> {100*len(ins)/len(opp):.0f}% of K377's recorded history sits inside its")
print(f"     current Neighboring Kingdom Leaderboard group.")
