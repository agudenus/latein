#!/usr/bin/env python3
"""
Tests on the derived pools (scripts/derive_pools.py must run first).

1. Pool sizes per KvK.
2. Within pool mixing: for pools spanning two or more published blocks or
   groups in force, compare cross unit pairings against uniform pairing
   inside the pool.
3. Rematch gaps: KvKs between repeat meetings of the same two kingdoms.
4. Record sorting: correlation of the two sides' prior battle win rate,
   against a within pool permutation null. Uses only results from earlier
   KvKs, so it is what matchmaking could have known.
"""
import json
from collections import Counter, defaultdict
import numpy as np
import pandas as pd

rng = np.random.default_rng(0)
m = pd.read_csv("data/matchups.csv")
pools = {int(k): v for k, v in json.load(open("data/pools_by_kvk.json")).items()}
al = json.load(open("data/groups.json"))["alignments"]


def pool_of(v, k):
    for a, b in pools[v]:
        if a <= k <= b:
            return (a, b)


m["pool"] = [pool_of(v, k) for v, k in zip(m.kvk, m.kingdom)]
real = m[m.opponent != 0]
pairs = real[real.kingdom < real.opponent]

print("1. Pool sizes (kingdoms that played or got a bye), pools of 10+")
for v in range(12, 20):
    sizes = m[m.kvk == v].groupby("pool").size()
    sizes = sizes[sizes >= 10]
    print(f"  KvK {v}: median {int(sizes.median())}, range {sizes.min()}-{sizes.max()}, n={len(sizes)}")


def unit(k, d):
    for a in al:
        lo, hi = a["group"]
        if lo <= k <= hi:
            if d >= a["effective"]:
                return (lo, hi)
            for blo, bhi in a["merged"]:
                if blo <= k <= bhi:
                    return (blo, bhi)
    return None


print("\n2. Mixing inside multi unit pools (published units only)")
obs = exp = n = 0
for (v, pool), x in m[m.kvk >= 10].groupby(["kvk", "pool"]):
    ks = list(x.kingdom)
    us = [unit(k, x.date.iloc[0]) for k in ks]
    if None in us or len(set(us)) < 2:
        continue
    c = Counter(us)
    N = len(ks)
    exp_cross = 1 - sum(s * (s - 1) for s in c.values()) / (N * (N - 1))
    p = pairs[(pairs.kvk == v) & (pairs.pool == pool)]
    cross = sum(unit(a, x.date.iloc[0]) != unit(b, x.date.iloc[0])
                for a, b in zip(p.kingdom, p.opponent))
    obs += cross
    exp += exp_cross * len(p)
    n += len(p)
    print(f"  KvK {v} pool {pool[0]}-{pool[1]} units {sorted(c)}: "
          f"cross {cross}/{len(p)} vs uniform {exp_cross*len(p):.1f}")
print(f"  TOTAL cross {obs}/{n} vs uniform {exp:.1f}")

print("\n3. Rematch gaps")
last = {}
gaps = Counter()
for r in pairs.sort_values("kvk").itertuples():
    key = (r.kingdom, r.opponent)
    if key in last:
        gaps[r.kvk - last[key]] += 1
    last[key] = r.kvk
print(" ", dict(sorted(gaps.items())), " total rematches", sum(gaps.values()))

print("\n4. Record sorting (prior battle win rate, kingdoms with 3+ prior KvKs)")
wins, games = defaultdict(int), defaultdict(int)
rows = []
for v in sorted(m.kvk.unique()):
    x = real[real.kvk == v]
    wr = {k: wins[k] / games[k] for k in set(x.kingdom) if games[k] >= 3}
    p = x[(x.kingdom < x.opponent) & x.kingdom.isin(wr) & x.opponent.isin(wr)]
    for r in p.itertuples():
        rows.append((v, r.pool, wr[r.kingdom], wr[r.opponent]))
    for r in x.itertuples():
        games[r.kingdom] += 1
        wins[r.kingdom] += r.battle == "W"
P = pd.DataFrame(rows, columns=["kvk", "pool", "a", "b"])
r_obs = np.corrcoef(np.r_[P.a, P.b], np.r_[P.b, P.a])[0, 1]
null = []
for _ in range(500):
    aa, bb = [], []
    for _, g in P.groupby(["kvk", "pool"]):
        vals = np.r_[g.a, g.b]
        rng.shuffle(vals)
        h = len(vals) // 2
        aa.append(vals[:h]); bb.append(vals[h:2*h])
    aa, bb = np.concatenate(aa), np.concatenate(bb)
    null.append(np.corrcoef(np.r_[aa, bb], np.r_[bb, aa])[0, 1])
null = np.array(null)
print(f"  pairs {len(P)}, observed r = {r_obs:+.3f}, within pool null "
      f"{null.mean():+.3f} +/- {null.std():.3f}, p = {(null >= r_obs).mean():.3f}")
for v in range(14, 20):
    q = P[P.kvk == v]
    print(f"  KvK {v}: r = {np.corrcoef(np.r_[q.a,q.b], np.r_[q.b,q.a])[0,1]:+.3f} (n={len(q)})")
