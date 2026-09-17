.PHONY: phase0 fetch extract freeze-splits run report test lint

# Runs `run` then `report` against already-committed data/interim +
# data/splits -- no network access required.
phase0: run report

fetch:
	uv run phase0 fetch

extract:
	uv run phase0 extract

freeze-splits:
	uv run phase0 freeze-splits

run:
	uv run phase0 run

report:
	uv run phase0 report

test:
	uv run pytest tests/ -x

lint:
	uv run ruff check .
	uv run ruff format --check .
