# Fixture file for the HORO-1127 release-gate corpus. Exists only so
# bounded reconnaissance has a real "pagination"-named path to match
# against prompts like "fix the off-by-one in the pagination logic".


def page_bounds(page: int, page_size: int, total: int) -> tuple[int, int]:
    start = page * page_size
    end = min(start + page_size, total)
    return start, end
