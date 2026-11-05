#!/usr/bin/env python3
"""
Normalise the ks-atlas KvK history into data/matchups.csv.

Input  data/atlas/AtlasKvKHistory.csv, one row per kingdom per KvK:
       id,kingdom_number,kvk_number,opponent_kingdom,prep_result,battle_result,kvk_date
Output data/matchups.csv, one row per kingdom per KvK:
       kvk,date,kingdom,opponent,prep,battle      (opponent 0 = bye)

Both files are gitignored. The repo is public and the dataset belongs to the
ks-atlas admin, so only scripts and aggregate results get committed.

kvk_date is blank for many rows (all of KvK 12 to 17). Every present date fits
KvK n = 2025-05-24 + 28*(n-1), so dates are filled from that and any
disagreement is reported.
"""
import sys
from datetime import date, timedelta
import pandas as pd

SRC = sys.argv[1] if len(sys.argv) > 1 else "data/atlas/AtlasKvKHistory.csv"
KVK1 = date(2025, 5, 24)


def kvk_date(n):
    return KVK1 + timedelta(days=28 * (n - 1))


df = pd.read_csv(SRC, keep_default_na=False)
df["date"] = df.kvk_number.map(lambda n: kvk_date(n).isoformat())
given = df[df.kvk_date != ""]
bad = given[given.kvk_date != given.date]
assert bad.empty, f"dates off the 28 day grid:\n{bad}"
assert not df.duplicated(["kingdom_number", "kvk_number"]).any()

out = df.rename(columns={"kvk_number": "kvk", "kingdom_number": "kingdom",
                         "opponent_kingdom": "opponent", "prep_result": "prep",
                         "battle_result": "battle"})
out = out[["kvk", "date", "kingdom", "opponent", "prep", "battle"]]
out = out.sort_values(["kvk", "kingdom"])

pairs = set(zip(out.kvk, out.kingdom, out.opponent))
real = out[out.opponent != 0]
one_sided = [(v, k, o) for v, k, o in zip(real.kvk, real.kingdom, real.opponent)
             if (v, o, k) not in pairs]
print(f"{len(out)} rows, KvK {out.kvk.min()} to {out.kvk.max()}, "
      f"{out.kingdom.nunique()} kingdoms, {(out.opponent == 0).sum()} byes, "
      f"{len(one_sided)} one sided matchups")
out.to_csv("data/matchups.csv", index=False)
