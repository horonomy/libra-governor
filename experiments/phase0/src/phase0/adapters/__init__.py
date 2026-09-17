"""Adapter registry: normalizes each submission's trajectory-cost schema.

Every adapter takes a distilled record (see sources/s3_trajs.py) and returns
a normalized dict with a fixed set of keys, regardless of which upstream
cost-stats shape (`info.model_stats` vs `llm_call_data`) the submission uses.
"""

from __future__ import annotations

from typing import Any, Callable

from . import epam, sweagent

NormalizedRecord = dict[str, Any]
Adapter = Callable[[dict[str, Any]], NormalizedRecord | None]

ADAPTERS: dict[str, Adapter] = {
    "sweagent": sweagent.adapt,
    "epam": epam.adapt,
}


def get_adapter(name: str) -> Adapter:
    try:
        return ADAPTERS[name]
    except KeyError as exc:
        raise ValueError(f"Unknown adapter '{name}'. Known: {sorted(ADAPTERS)}") from exc
