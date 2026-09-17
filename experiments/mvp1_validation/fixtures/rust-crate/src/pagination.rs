// Fixture file for the HORO-1127 release-gate corpus. Exists only so
// bounded reconnaissance has a real "pagination"-named path to match
// against prompts like "fix the off-by-one in the pagination logic".

pub fn page_bounds(page: usize, page_size: usize, total: usize) -> (usize, usize) {
    let start = page * page_size;
    let end = (start + page_size).min(total);
    (start, end)
}

pub fn lib_root() {}
