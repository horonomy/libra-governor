"""Optional stretch adapter for epam-ai-run-* submissions: llm_call_data schema.

Not part of the 6 required SWE-agent submissions and not exercised by the
default config's `submissions.required` list -- included per spec as a
stretch adapter. The exact `llm_call_data` shape was not independently
re-verified against a live S3 object during this implementation (only the
6 SWE-agent submissions were live-probed), so this is a best-effort,
defensive parse: any submission using it that doesn't match this shape
degrades to `None` (skipped), never a fabricated record. See PROVENANCE.md.

Expected distilled record shape (analogous to sweagent.py, but with
`llm_call_data` in place of `model_stats`):
    {"instance_id": ..., "submission": ..., "exit_status": <str|None>,
     "llm_call_data": {"total_cost": float, "tokens_sent": int,
                        "tokens_received": int, "num_calls": int}}
"""

from __future__ import annotations

from typing import Any


def adapt(record: dict[str, Any]) -> dict[str, Any] | None:
    call_data = record.get("llm_call_data")
    if not call_data:
        return None
    try:
        instance_cost_usd = float(call_data["total_cost"])
    except (KeyError, TypeError, ValueError):
        return None

    exit_status = record.get("exit_status") or ""
    return {
        "instance_id": record["instance_id"],
        "submission": record["submission"],
        "instance_cost_usd": instance_cost_usd,
        "tokens_sent": int(call_data.get("tokens_sent", 0) or 0),
        "tokens_received": int(call_data.get("tokens_received", 0) or 0),
        "api_calls": int(call_data.get("num_calls", 0) or 0),
        "exit_status": exit_status,
    }
