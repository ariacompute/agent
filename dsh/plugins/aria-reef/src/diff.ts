/**
 * Minimal unified-diff renderer. Trims the common prefix/suffix and emits one
 * hunk — enough to make recipe proposals reviewable without a diff dependency.
 */
export function unifiedDiff(oldText: string, newText: string, path: string): string {
  const before = oldText.length === 0 ? [] : oldText.split("\n");
  const after = newText.length === 0 ? [] : newText.split("\n");

  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) {
    start += 1;
  }
  let endBefore = before.length;
  let endAfter = after.length;
  while (endBefore > start && endAfter > start && before[endBefore - 1] === after[endAfter - 1]) {
    endBefore -= 1;
    endAfter -= 1;
  }

  const header = `--- a/${path}\n+++ b/${path}`;
  if (before.length === 0 && after.length === 0) {
    return `${header}\n`;
  }

  const lines: string[] = [];
  const contextBefore = Math.max(0, start);
  const beforeCount = endBefore - start;
  const afterCount = endAfter - start;
  lines.push(
    `@@ -${beforeCount === 0 ? contextBefore : contextBefore + 1},${beforeCount} +${afterCount === 0 ? contextBefore : contextBefore + 1},${afterCount} @@`,
  );
  for (let i = 0; i < beforeCount; i += 1) {
    lines.push(`-${before[start + i]}`);
  }
  for (let i = 0; i < afterCount; i += 1) {
    lines.push(`+${after[start + i]}`);
  }
  const tail = before
    .slice(endBefore)
    .map((line) => ` ${line}`)
    .join("\n");
  if (tail.length > 0) {
    lines.push(tail);
  }
  return `${header}\n${lines.join("\n")}\n`;
}
