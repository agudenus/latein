#!/usr/bin/env python3
"""
Pre-registered KvK 19 predictions for K377's group, K236 to K417.

Stage 1 only. No power data exists yet, so inside the group every eligible
kingdom is equally likely. The point of writing this down before matchmaking
(2026-10-10) is that the real pairings can then be scored against a record
that could not have been tuned to them.

Deliberately NOT used: any distance decay kernel. See CLAUDE.md section 3.
"""
import json

GROUP = (236, 417)               # in force since 2026-06-03
BLOCKS = [(236, 309), (310, 417)] # the two blocks merged on 2026-06-03
ME = 377
COOLDOWN = 3                     # rematch cooldown in KvKs
MATCHMAKING = "2026-10-10"

groups = json.load(open("data/groups.json"))
history = groups["opponents"].get(str(ME), [])

members = list(range(GROUP[0], GROUP[1] + 1))
n = len(members)
others = [k for k in members if k != ME]
a, b = (hi - lo + 1 for lo, hi in BLOCKS)
my_block = next(blk for blk in BLOCKS if blk[0] <= ME <= blk[1])
other_block = next(blk for blk in BLOCKS if blk != my_block)
other_size = other_block[1] - other_block[0] + 1

# K377's last three opponents (KvK 16, 17, 18) are excluded by the cooldown,
# but which ones they are is not recoverable from the frozen, unordered list.
eligible = len(others) - COOLDOWN

out = {
    "registered": "2026-09-26",
    "kvk": 19,
    "matchmaking": MATCHMAKING,
    "group": GROUP,
    "group_size": n,
    "k377": {
        "P_opponent_inside_group": ">= 0.95",
        "candidates_before_cooldown": len(others),
        "candidates_after_cooldown": eligible,
        "P_each_candidate_uniform": round(1 / eligible, 5),
        "P_opponent_in_other_block": {
            "block": other_block,
            "uniform_null": round(other_size / eligible, 3),
            "note": "if pairing is uniform inside the group. Well below this "
                    "means Stage 2 keeps some block or power locality.",
        },
        "known_past_opponents_in_group": sorted(o for o in history
                                                if GROUP[0] <= o <= GROUP[1]),
        "cooldown_exclusions": "KvK 16, 17, 18 opponents, to be supplied by Alex",
    },
    "group_wide": {
        "expected_pairings": n // 2,
        "leak_outside_group_max": 1,
        "cross_block_share_uniform_null": round(2 * a * b / (n * (n - 1)), 3),
        "rematches_within_cooldown": 0,
        "byes_expected": 0 if n % 2 == 0 else 1,
        "needs": "full KvK 19 pairings, from the Atlas data or the optimizer "
                 "once its opponent lists unfreeze",
    },
}
json.dump(out, open("predictions/kvk19_k236-417.json", "w"), indent=2)
print(json.dumps(out, indent=2))
