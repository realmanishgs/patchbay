/**
 * Subsequence fuzzy match. Returns -1 when every character of `query` cannot be found in
 * order inside `text`. Higher scores are better: consecutive hits and early hits score more.
 */
export function fuzzyScore(query: string, text: string): number {
  const q = query.toLowerCase().replace(/\s+/g, "");
  if (q.length === 0) return 0;
  const t = text.toLowerCase();
  let score = 0;
  let cursor = 0;
  let streak = 0;
  for (const ch of q) {
    const found = t.indexOf(ch, cursor);
    if (found === -1) return -1;
    const gap = found - cursor;
    streak = gap === 0 && cursor > 0 ? streak + 1 : 1;
    score += 10 + streak * 4 - Math.min(gap, 10);
    if (found === 0) score += 8;
    cursor = found + 1;
  }
  return score;
}
