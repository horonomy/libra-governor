"""Asserts censored rows are kept (never dropped) and the censored flag is correct."""

from __future__ import annotations

from pathlib import Path

import pandas as pd
import pytest

from phase0.pipeline import is_censored

FIXTURE_PATH = Path(__file__).parent / "fixtures" / "mini.parquet"


@pytest.fixture()
def df() -> pd.DataFrame:
    return pd.read_parquet(FIXTURE_PATH)


def test_censored_rows_are_present_in_the_dataset(df):
    """The fixture is built with a nonzero exit_cost/exit_context rate --
    this asserts none of those rows were filtered out anywhere upstream.
    """
    assert df["censored"].sum() > 0
    assert df["censored"].sum() < len(df)  # not ALL rows censored either


@pytest.mark.parametrize(
    "exit_status,expected",
    [
        ("submitted", False),
        ("submitted (exit_cost)", True),
        ("submitted (exit_context)", True),
        ("", False),
        (None, False),
        ("exit_cost", True),
    ],
)
def test_is_censored(exit_status, expected):
    assert is_censored(exit_status) is expected


def test_censored_flag_matches_recomputed_value(df):
    recomputed = df["exit_status"].apply(is_censored)
    assert (recomputed == df["censored"]).all()


def test_censored_rows_retain_a_lower_bound_cost(df):
    """Censored rows must still carry instance_cost_usd (a lower bound on
    true cost, per spec) rather than being nulled out.
    """
    censored_rows = df[df["censored"]]
    assert censored_rows["instance_cost_usd"].notna().all()
    assert (censored_rows["instance_cost_usd"] >= 0).all()
