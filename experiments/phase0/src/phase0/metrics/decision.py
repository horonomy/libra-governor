"""Decision-utility metric: admission_regret().

Policy under evaluation: admit a task iff its predicted p80 cost is within
budget. This is a Phase 0 feasibility probe of that policy, not the actual
admission policy libra-governor's daemon/ledger crates will run.
"""

from __future__ import annotations

import numpy as np


def admission_regret(
    pred_q80: np.ndarray,
    actual: np.ndarray,
    budgets: list[float],
    censored: np.ndarray,
    resolved: np.ndarray | None = None,
    forgone_success_penalty_usd: float = 5.0,
) -> list[dict]:
    """For each budget, evaluate the "admit iff pred_q80 <= budget" policy.

    Returns one dict per budget with:
      budget_usd, admit_rate, false_admit_count, false_admit_excess_usd,
      false_reject_count, forgone_success_count, regret_usd
    """
    pred_q80 = np.asarray(pred_q80, dtype=float)
    actual = np.asarray(actual, dtype=float)
    censored = np.asarray(censored, dtype=bool)
    n = len(pred_q80)
    if resolved is None:
        resolved = np.zeros(n, dtype=bool)
    else:
        resolved = np.asarray(resolved, dtype=bool)

    rows = []
    for budget in budgets:
        admit = pred_q80 <= budget
        admit_rate = float(np.mean(admit)) if n else float("nan")

        # False admit: we predicted it fits the budget but actual cost
        # exceeded it. Censored rows are a lower bound on actual cost, so a
        # censored row whose recorded (lower-bound) actual already exceeds
        # budget still counts -- true excess can only be larger.
        false_admit = admit & (actual > budget)
        false_admit_excess_usd = float(np.sum(actual[false_admit] - budget))
        false_admit_count = int(np.sum(false_admit))

        false_reject = (~admit) & (actual <= budget)
        false_reject_count = int(np.sum(false_reject))
        forgone_success_count = int(np.sum(false_reject & resolved))

        regret_usd = false_admit_excess_usd + forgone_success_penalty_usd * forgone_success_count

        rows.append(
            {
                "budget_usd": budget,
                "admit_rate": admit_rate,
                "false_admit_count": false_admit_count,
                "false_admit_excess_usd": false_admit_excess_usd,
                "false_reject_count": false_reject_count,
                "forgone_success_count": forgone_success_count,
                "regret_usd": regret_usd,
            }
        )
    return rows
