#!/usr/bin/env python3
"""
Stage 2 backtest using battle record only (no power data exists).

For each kingdom in KvK 15 to 19: candidates are the other kingdoms in its
ACTUAL pool (oracle, since pool prediction is a separate open problem), minus
the rematch cooldown (opponents from the previous 2 KvKs). Candidates are
ranked by closeness of prior battle win rate. Score = rank of the real
opponent, compared with uniform random ranking.
"""
from collections import defaultdict
import json
import numpy as np
import pandas as pd

m = pd.read_csv("data/matchups.csv")
pools = {int(k): v for k, v in json.load(open("data/pools_by_kvk.json")).items()}
wins, games = defaultdict(int), defaultdict(int)
hist = defaultdict(dict)
res = []
for v in sorted(m.kvk.unique()):
    x = m[m.kvk == v]
    if v >= 15:
        for a, b in pools[v]:
            px = x[x.kingdom.between(a, b)]
            ks = px.kingdom.to_numpy()
            # smoothed win rate, prior 1 win 1 loss
            wr = {k: (wins[k] + 1) / (games[k] + 2) for k in ks}
            for r in px[px.opponent != 0].itertuples():
                ban = {hist[r.kingdom].get(v - 1), hist[r.kingdom].get(v - 2), r.kingdom}
                cand = [k for k in ks if k not in ban]
                if r.opponent not in cand or len(cand) < 5:
                    continue
                d = np.array([abs(wr[k] - wr[r.kingdom]) for k in cand])
                d = d + np.random.default_rng(r.kingdom * 100 + v).random(len(d)) * 1e-9
                rank = 1 + (d < d[cand.index(r.opponent)]).sum()
                res.append((v, len(cand), rank))
    for r in x.itertuples():
        hist[r.kingdom][v] = r.opponent
        if r.opponent != 0:
            games[r.kingdom] += 1
            wins[r.kingdom] += r.battle == "W"

R = pd.DataFrame(res, columns=["kvk", "n", "rank"])
for v, g in list(R.groupby("kvk")) + [("all", R)]:
    print(f"KvK {v}: n={len(g)}, median pool {int(g.n.median())}, "
          f"top1 {(g['rank']==1).mean():.1%} (uniform {(1/g.n).mean():.1%}), "
          f"top5 {(g['rank']<=5).mean():.1%} (uniform {(5/g.n).clip(upper=1).mean():.1%}), "
          f"top10 {(g['rank']<=10).mean():.1%} (uniform {(10/g.n).clip(upper=1).mean():.1%}), "
          f"rank pct {(g['rank']/g.n).mean():.2f} (uniform ~0.50)")
