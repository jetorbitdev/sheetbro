import type { Merge, Rect } from "./api";

/** The merged range containing (row, col), if any. */
export function mergeAt(merges: Merge[], row: number, col: number): Merge | undefined {
  return merges.find(
    (m) => row >= m.row && row < m.row + m.height && col >= m.col && col < m.col + m.width,
  );
}

/** Top-left cell of the merge containing (row, col), or the cell itself. */
export function anchorOf(merges: Merge[], row: number, col: number): Merge {
  return mergeAt(merges, row, col) ?? { row, col, height: 1, width: 1 };
}

/** Grows `rect` until no merged range is cut by its border (selecting part of a merge selects all of it). */
export function expandRect(rect: Rect, merges: Merge[]): Rect {
  let { row, col, height, width } = rect;
  for (let changed = true; changed; ) {
    changed = false;
    for (const m of merges) {
      const overlaps =
        m.row < row + height && m.row + m.height > row && m.col < col + width && m.col + m.width > col;
      if (!overlaps) continue;
      const r0 = Math.min(row, m.row);
      const c0 = Math.min(col, m.col);
      const r1 = Math.max(row + height, m.row + m.height);
      const c1 = Math.max(col + width, m.col + m.width);
      if (r0 !== row || c0 !== col || r1 - r0 !== height || c1 - c0 !== width) {
        row = r0;
        col = c0;
        height = r1 - r0;
        width = c1 - c0;
        changed = true;
      }
    }
  }
  return { row, col, height, width };
}
