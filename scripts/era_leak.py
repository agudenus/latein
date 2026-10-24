#!/usr/bin/env python3
"""
Era aware leak test (CLAUDE.md section 7 step 2).

Each matchup is scored against the group boundaries IN FORCE ON ITS DATE:
the merged group on or after an alignment's effective date, the constituent
block before it. Prediction under test: leak collapses to near zero for every
cohort once eras are respected.
"""
import json
import pandas as pd

m = pd.read_csv("data/matchups.csv")
m = m[m.opponent != 0]
al = json.load(open("data/groups.json"))["alignments"]


def boundary(k, d):
    for a in al:
        lo, hi = a["group"]
        if lo <= k <= hi:
            if d >= a["effective"]:
                return tuple(a["group"]), "merged"
            for blo, bhi in a["merged"]:
                if blo <= k <= bhi:
                    return (blo, bhi), "block"
    return None, None


rows = []
for r in m.itertuples():
    g, era = boundary(r.kingdom, r.date)
    if g is None:
        continue
    rows.append(dict(kvk=r.kvk, kingdom=r.kingdom, opponent=r.opponent,
                     group=f"{g[0]}-{g[1]}", era=era,
                     leak=not (g[0] <= r.opponent <= g[1])))
R = pd.DataFrame(rows)

print("Leak by boundary in force and KvK (rows = kingdom sides)")
t = R.pivot_table(index="group", columns="kvk", values="leak",
                  aggfunc=lambda s: f"{s.sum()}/{len(s)}")
pd.set_option("display.width", 250)
print(t.fillna("").to_string())

print("\nTotals by era")
s = R.groupby(["group", "era"]).leak.agg(["sum", "size"])
s["pct"] = (100 * s["sum"] / s["size"]).round(1)
print(s.to_string())
R.to_csv("data/era_leak_rows.csv", index=False)  # gitignored, row level
