#!/usr/bin/env python3
"""Score predictions/kvk19_k236-417.json against the Atlas KvK 19 pairings."""
import pandas as pd

m = pd.read_csv("data/matchups.csv")
x = m[(m.kvk == 19) & m.kingdom.between(236, 417)]
real = x[x.opponent != 0]
pairs = real[(real.kingdom < real.opponent) | ~real.opponent.between(236, 417)]
me = x[x.kingdom == 377].iloc[0]
hist = m[(m.kingdom == 377) & (m.kvk.between(16, 18))].opponent.tolist()

print("K377")
print(f"  1 opponent inside 236-417 : K{me.opponent} -> {236 <= me.opponent <= 417}")
print(f"  2 not a KvK 16-18 opponent {hist}: {me.opponent not in hist}")
print(f"  4 opponent in 236-309     : {236 <= me.opponent <= 309} (null 41.6%)")
out = pairs[~pairs.opponent.between(236, 417)]
print("Group wide")
print(f"  5 pairings leaving 236-417 : {len(out)} of {len(pairs)} (predicted at most 1)")
inside = pairs[pairs.opponent.between(236, 417)]
cross = ((inside.kingdom <= 309) != (inside.opponent <= 309)).sum()
print(f"  8 crossing 309/310 among in-group pairings: {cross}/{len(inside)} "
      f"= {cross/len(inside):.1%} (null 48.5%)")
rem = 0
for r in real.itertuples():
    prev = m[(m.kingdom == r.kingdom) & m.kvk.between(17, 18)].opponent
    rem += r.opponent in set(prev)
print(f"  6 rematches within cooldown: {rem}")
print(f"  7 byes: {(x.opponent == 0).sum()} of {len(x)} kingdoms")
print(f"  leaked opponents range: K{out.opponent.min()} to K{out.opponent.max()}")
