// Fixture file for the HORO-1127 release-gate corpus. Exists only so
// bounded reconnaissance has a real "pagination"-named path to match
// against prompts like "fix the off-by-one in the pagination logic".

export function pageBounds(
  page: number,
  pageSize: number,
  total: number
): [number, number] {
  const start = page * pageSize;
  const end = Math.min(start + pageSize, total);
  return [start, end];
}
