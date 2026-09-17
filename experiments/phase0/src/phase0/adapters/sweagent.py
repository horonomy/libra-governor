"""Adapter for SWE-agent-family submissions: info.model_stats schema.

Distilled record shape (from sources/s3_trajs.py):
    {"instance_id": ..., "submission": ..., "exit_status": <str|None>,
     "model_stats": {"instance_cost": float, "tokens_sent": int,
                      "tokens_received": int, "api_calls": int}}
"""

from __future__ import annotations

from typing import Any


def adapt(record: dict[str, Any]) -> dict[str, Any] | None:
    stats = record.get("model_stats")
    if not stats:
        return None
    try:
        instance_cost_usd = float(stats["instance_cost"])
    except (KeyError, TypeError, ValueError):
        return None

    exit_status = record.get("exit_status") or ""
    return {
        "instance_id": record["instance_id"],
        "submission": record["submission"],
        "instance_cost_usd": instance_cost_usd,
        "tokens_sent": int(stats.get("tokens_sent", 0) or 0),
        "tokens_received": int(stats.get("tokens_received", 0) or 0),
        "api_calls": int(stats.get("api_calls", 0) or 0),
        "exit_status": exit_status,
    }
