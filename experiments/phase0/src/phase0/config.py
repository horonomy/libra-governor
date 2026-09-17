"""Loading and content-hashing of configs/phase0.yaml."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG_PATH = REPO_ROOT / "configs" / "phase0.yaml"


@dataclass(frozen=True)
class Config:
    path: Path
    raw: dict[str, Any]
    sha256: str

    def __getitem__(self, key: str) -> Any:
        return self.raw[key]

    def get(self, key: str, default: Any = None) -> Any:
        return self.raw.get(key, default)


def load_config(path: str | Path | None = None) -> Config:
    config_path = Path(path) if path is not None else DEFAULT_CONFIG_PATH
    text = config_path.read_text(encoding="utf-8")
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    raw = yaml.safe_load(text)
    return Config(path=config_path, raw=raw, sha256=digest)


def required_submissions(cfg: Config) -> list[dict[str, Any]]:
    return list(cfg["submissions"]["required"])


def optional_submissions(cfg: Config) -> list[dict[str, Any]]:
    return list(cfg["submissions"].get("optional", []))


def all_submissions(cfg: Config, include_optional: bool = False) -> list[dict[str, Any]]:
    subs = required_submissions(cfg)
    if include_optional:
        subs = subs + optional_submissions(cfg)
    return subs
