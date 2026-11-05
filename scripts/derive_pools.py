#!/usr/bin/env python3
"""
Derive each KvK's actual matchmaking pools from the matchups alone.

A cut sits between kingdom x and x+1 when no matchup in that KvK pairs a
kingdom <= x with one > x. The pools are the maximal number ranges between
cuts. No group map is assumed, so the result can be compared against the
published Neighboring Kingdom Leaderboard groups afterwards.
"""
import json
import numpy as np
import pandas as pd

m = pd.read_csv("data/matchups.csv")
out = {}
for v, x in m.groupby("kvk"):
    played = sorted(set(x.kingdom))
    real = x[x.opponent != 0]
    lo = np.minimum(real.kingdom, real.opponent).to_numpy()
    hi = np.maximum(real.kingdom, real.opponent).to_numpy()
    top = max(played)
    # cover[i] = number of matchups spanning the gap between i and i+1
    diff = np.zeros(top + 2, int)
    np.add.at(diff, lo, 1)
    np.add.at(diff, hi, -1)
    cover = np.cumsum(diff)
    pools, start = [], played[0]
    for k in played[:-1]:
        if cover[k] == 0:
            nxt = next(p for p in played if p > k)
            pools.append([start, k])
            start = nxt
    pools.append([start, top])
    out[int(v)] = pools

json.dump(out, open("data/pools_by_kvk.json", "w"), indent=1)
for v, pools in out.items():
    big = [f"{a}-{b}" for a, b in pools if b - a >= 5]
    small = sum(1 for a, b in pools if b - a < 5)
    print(f"KvK {v:2d}: {len(pools):3d} pools  " + "  ".join(big) +
          (f"  (+{small} tiny)" if small else ""))
