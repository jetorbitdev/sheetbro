import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, CellData, columnName, FormulaPreview, Layout, Rect } from "./api";
import type { FormulaRef } from "./formula";
import { FormulaMirror } from "./FormulaMirror";
import { anchorOf, expandRect } from "./merges";

export interface Sel {
  /** anchor = active cell */
  ar: number;
  ac: number;
  /** focus = the moving corner */
  fr: number;
  fc: number;
}

export const selRect = (s: Sel) => {
  const row = Math.min(s.ar, s.fr);
  const col = Math.min(s.ac, s.fc);
  return {
    row,
    col,
    height: Math.abs(s.ar - s.fr) + 1,
    width: Math.abs(s.ac - s.fc) + 1,
  };
};

const HEADER_W = 52;
const HEADER_H = 30;
const PREVIEW_H = 26;
const ARROWS: Record<string, [number, number]> = {
  ArrowUp: [-1, 0],
  ArrowDown: [1, 0],
  ArrowLeft: [0, -1],
  ArrowRight: [0, 1],
};
const OVERSCAN_ROWS = 30;
const OVERSCAN_COLS = 4;
export const SANS = '"IBM Plex Sans", -apple-system, sans-serif';
export const MONO = '"IBM Plex Mono", ui-monospace, monospace';
/** how close (px) to a column border the pointer must be to grab it */
const RESIZE_SLOP = 4;
/** how far (px) outside and inside the selection's edge the pointer still grabs it to move the cells */
const GRAB_OUT = 2;
const GRAB_IN = 4;
const MIN_COL_W = 24;
const MAX_COL_W = 2000;

interface Props {
  sheet: number;
  layout: Layout;
  rows: number;
  cols: number;
  sel: Sel;
  setSel: (s: Sel) => void;
  /** bumped by the parent whenever workbook data changes */
  rev: number;
  editing: { value: string } | null;
  setEditValue: (v: string) => void;
  startEdit: () => void;
  commitEdit: (dr: number, dc: number) => void;
  cancelEdit: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  /** 1-based column numbers */
  onResizeColumns: (cols: number[], width: number) => void;
  onAutofitColumns: (cols: number[]) => void;
  /** formula point mode: a click or drag on cells inserts a reference instead of ending the edit */
  canPoint: () => boolean;
  onPoint: (rect: Rect) => void;
  /** arrow keys while typing a formula; true when they picked a cell instead of moving the caret */
  onPointKey: (dr: number, dc: number, extend: boolean) => boolean;
  /** the cell the arrow keys are on while picking a reference */
  pointFocus: { row: number; col: number } | null;
  /** references in the formula being edited, boxed on the grid in their colors */
  refs: FormulaRef[];
  /** what the formula being edited would show */
  preview: FormulaPreview | null;
  /** right-click: the selection already covers what was clicked; `at` is the pointer in viewport pixels */
  onContextMenu: (at: { left: number; top: number }) => void;
  /** the selection was dragged by its edge: `from` now goes with its top-left corner at (row, col) */
  onMoveCells: (from: Rect, row: number, col: number) => void;
}

/** First index i in [0, n) with prefix[i + 1] > v, i.e. the item containing offset v. */
function locate(prefix: Float64Array, v: number): number {
  let lo = 0;
  let hi = prefix.length - 2;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (prefix[mid + 1] <= v) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export function Grid(p: Props) {
  const { layout, rows, cols } = p;
  const scroller = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [size, setSize] = useState({ w: 800, h: 600 });
  const [scroll, setScroll] = useState({ x: 0, y: 0 });
  const [, forceDraw] = useState(0);
  const data = useRef<{
    sheet: number;
    r0: number;
    r1: number;
    c0: number;
    c1: number;
    cells: Map<number, CellData>;
  } | null>(null);
  const fetchSeq = useRef(0);
  const dragging = useRef<null | "cells" | "cols" | "rows" | "resize" | "point" | "move">(null);
  const pointAnchor = useRef({ row: 1, col: 1 });
  // live width while dragging a column border; cleared once the new layout arrives
  const [resize, setResize] = useState<{ col: number; width: number } | null>(null);
  const resizeStart = useRef({ col: 0, x: 0, width: 0 });
  const [hoverBorder, setHoverBorder] = useState<number | null>(null);
  // the selection's edge is under the pointer (a hand), or the cells are being carried to `moving`
  const [hoverEdge, setHoverEdge] = useState(false);
  const [moving, setMoving] = useState<Rect | null>(null);
  const moveStart = useRef<{ row: number; col: number; from: Rect }>({
    row: 1,
    col: 1,
    from: { row: 1, col: 1, height: 1, width: 1 },
  });
  useEffect(() => setResize(null), [layout]);

  // Prefix sums give O(log n) hit-testing and O(1) cell geometry.
  const colX = useMemo(() => {
    const a = new Float64Array(cols + 1);
    for (let c = 0; c < cols; c++) {
      const w = resize?.col === c + 1 ? resize.width : layout.col_widths[c] ?? 90;
      a[c + 1] = a[c] + w;
    }
    return a;
  }, [layout, cols, resize]);
  const rowY = useMemo(() => {
    const overrides = new Map(layout.row_heights);
    const a = new Float64Array(rows + 1);
    for (let r = 0; r < rows; r++) {
      a[r + 1] = a[r] + (overrides.get(r + 1) ?? layout.default_row_height);
    }
    return a;
  }, [layout, rows]);

  const totalW = HEADER_W + colX[cols];
  const totalH = HEADER_H + rowY[rows];

  // ---- visible window (0-based indexes)
  const vis = useMemo(() => {
    const gx = scroll.x;
    const gy = scroll.y;
    const c0 = locate(colX, gx);
    const c1 = locate(colX, gx + size.w - HEADER_W);
    const r0 = locate(rowY, gy);
    const r1 = locate(rowY, gy + size.h - HEADER_H);
    return { r0, r1, c0, c1 };
  }, [scroll, size, colX, rowY]);

  // ---- resize observer
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const ro = new ResizeObserver(() =>
      setSize({ w: el.clientWidth, h: el.clientHeight }),
    );
    ro.observe(el);
    setSize({ w: el.clientWidth, h: el.clientHeight });
    return () => ro.disconnect();
  }, []);

  // ---- data fetching: refetch when the window leaves the cached range or data changed
  const lastRev = useRef(-1);
  useEffect(() => {
    const d = data.current;
    let r0 = vis.r0 + 1;
    const r1 = vis.r1 + 1;
    let c0 = vis.c0 + 1;
    const c1 = vis.c1 + 1;
    // a merged cell's content lives in its anchor, which may be scrolled out of view
    for (const m of layout.merges) {
      if (m.row <= r1 && m.row + m.height > r0 && m.col <= c1 && m.col + m.width > c0) {
        r0 = Math.min(r0, m.row);
        c0 = Math.min(c0, m.col);
      }
    }
    const covered =
      d &&
      d.sheet === p.sheet &&
      lastRev.current === p.rev &&
      r0 >= d.r0 &&
      r1 <= d.r1 &&
      c0 >= d.c0 &&
      c1 <= d.c1;
    if (covered) return;
    const want = {
      r0: Math.max(1, r0 - OVERSCAN_ROWS),
      r1: Math.min(rows, r1 + OVERSCAN_ROWS),
      c0: Math.max(1, c0 - OVERSCAN_COLS),
      c1: Math.min(cols, c1 + OVERSCAN_COLS),
    };
    const seq = ++fetchSeq.current;
    const rev = p.rev;
    api.getViewport(p.sheet, want.r0, want.r1, want.c0, want.c1).then((list) => {
      if (seq !== fetchSeq.current) return; // a newer request superseded this one
      const cells = new Map<number, CellData>();
      for (const c of list) cells.set(c.r * 100000 + c.c, c);
      data.current = { sheet: p.sheet, ...want, cells };
      lastRev.current = rev;
      forceDraw((n) => n + 1);
    });
  }, [vis, p.sheet, p.rev, rows, cols, layout.merges]);

  // ---- keep the focus cell on screen
  const reveal = (row: number, col: number, x = true, y = true) => {
    const el = scroller.current;
    if (!el) return;
    const r = row - 1;
    const c = col - 1;
    if (r < 0 || c < 0 || r >= rows || c >= cols) return;
    const left = colX[c];
    const right = colX[c + 1];
    const top = rowY[r];
    const bottom = rowY[r + 1];
    const viewW = el.clientWidth - HEADER_W;
    const viewH = el.clientHeight - HEADER_H;
    if (x) {
      if (left < el.scrollLeft) el.scrollLeft = left;
      else if (right > el.scrollLeft + viewW) el.scrollLeft = right - viewW;
    }
    if (y) {
      if (top < el.scrollTop) el.scrollTop = top;
      else if (bottom > el.scrollTop + viewH) el.scrollTop = bottom - viewH;
    }
  };

  useEffect(() => {
    // A selection spanning a whole axis (select all, header clicks) has its
    // focus on the last row/column; chasing it would jump to the far edge.
    const allRows = Math.min(p.sel.ar, p.sel.fr) === 1 && Math.max(p.sel.ar, p.sel.fr) === rows;
    const allCols = Math.min(p.sel.ac, p.sel.fc) === 1 && Math.max(p.sel.ac, p.sel.fc) === cols;
    reveal(p.sel.fr, p.sel.fc, !allCols, !allRows);
  }, [p.sel.ar, p.sel.ac, p.sel.fr, p.sel.fc, colX, rowY, rows, cols]); // eslint-disable-line react-hooks/exhaustive-deps

  // the cell picked with the arrow keys while typing a formula
  useEffect(() => {
    if (p.pointFocus) reveal(p.pointFocus.row, p.pointFocus.col);
  }, [p.pointFocus?.row, p.pointFocus?.col]); // eslint-disable-line react-hooks/exhaustive-deps

  // ---- drawing
  const draw = useCallback(() => {
    const cv = canvas.current;
    if (!cv) return;
    const dpr = window.devicePixelRatio || 1;
    const { w, h } = size;
    if (cv.width !== Math.round(w * dpr) || cv.height !== Math.round(h * dpr)) {
      cv.width = Math.round(w * dpr);
      cv.height = Math.round(h * dpr);
    }
    const ctx = cv.getContext("2d")!;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const bg = cssVar("--grid-bg");
    const line = cssVar("--grid-line");
    const headBg = cssVar("--head-bg");
    const headBgSel = cssVar("--head-bg-sel");
    const headText = cssVar("--head-text");
    const headTextSel = cssVar("--accent");
    const text = cssVar("--text");
    const accent = cssVar("--accent");
    const selFill = cssVar("--sel-fill");

    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, w, h);

    const sx = scroll.x;
    const sy = scroll.y;
    const { r0, r1, c0, c1 } = vis;
    const colScreen = (c: number) => HEADER_W + colX[c] - sx;
    const rowScreen = (r: number) => HEADER_H + rowY[r] - sy;

    // body clip
    ctx.save();
    ctx.beginPath();
    ctx.rect(HEADER_W, HEADER_H, w - HEADER_W, h - HEADER_H);
    ctx.clip();

    // fills first, then grid lines, merged cells, text
    const d = data.current && data.current.sheet === p.sheet ? data.current : null;
    const merges = layout.merges;
    const visMerges = merges.filter(
      (m) =>
        m.row - 1 <= r1 && m.row - 1 + m.height > r0 && m.col - 1 <= c1 && m.col - 1 + m.width > c0,
    );
    // cells swallowed by a visible merge (clipped to the window) only render through it
    const swallowed = new Set<number>();
    for (const m of visMerges) {
      const rEnd = Math.min(m.row - 1 + m.height - 1, r1);
      const cEnd = Math.min(m.col - 1 + m.width - 1, c1);
      for (let r = Math.max(m.row - 1, r0); r <= rEnd; r++) {
        for (let c = Math.max(m.col - 1, c0); c <= cEnd; c++) {
          swallowed.add((r + 1) * 100000 + (c + 1));
        }
      }
    }
    const boxOf = (m: { row: number; col: number; height: number; width: number }) => {
      const cEnd = Math.min(m.col - 1 + m.width, cols);
      const rEnd = Math.min(m.row - 1 + m.height, rows);
      return {
        x: colScreen(m.col - 1),
        y: rowScreen(m.row - 1),
        w: colX[cEnd] - colX[m.col - 1],
        h: rowY[rEnd] - rowY[m.row - 1],
      };
    };

    if (d) {
      for (let r = r0; r <= r1; r++) {
        for (let c = c0; c <= c1; c++) {
          const key = (r + 1) * 100000 + (c + 1);
          const cell = d.cells.get(key);
          if (cell?.fill && !swallowed.has(key)) {
            ctx.fillStyle = cell.fill;
            ctx.fillRect(
              colScreen(c),
              rowScreen(r),
              colX[c + 1] - colX[c],
              rowY[r + 1] - rowY[r],
            );
          }
        }
      }
    }

    ctx.strokeStyle = line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let c = c0; c <= c1 + 1 && c <= cols; c++) {
      const x = Math.round(colScreen(c)) - 0.5;
      ctx.moveTo(x, HEADER_H);
      ctx.lineTo(x, h);
    }
    for (let r = r0; r <= r1 + 1 && r <= rows; r++) {
      const y = Math.round(rowScreen(r)) - 0.5;
      ctx.moveTo(HEADER_W, y);
      ctx.lineTo(w, y);
    }
    ctx.stroke();

    // merged cells hide the grid lines running through them
    for (const m of visMerges) {
      const b = boxOf(m);
      ctx.fillStyle = d?.cells.get(m.row * 100000 + m.col)?.fill ?? bg;
      ctx.fillRect(b.x, b.y, b.w, b.h);
      ctx.strokeStyle = line;
      ctx.lineWidth = 1;
      const x0 = Math.round(b.x) - 0.5;
      const y0 = Math.round(b.y) - 0.5;
      ctx.strokeRect(x0, y0, Math.round(b.x + b.w) - Math.round(b.x), Math.round(b.y + b.h) - Math.round(b.y));
    }

    const pad = 8;
    const drawText = (cell: CellData, x: number, y: number, cw: number, rh: number) => {
      ctx.textBaseline = "middle";
      ctx.font = `${cell.italic ? "italic " : ""}${cell.bold ? "600 " : "400 "}13px ${cell.kind === "num" ? MONO : SANS}`;
      ctx.fillStyle = cell.kind === "err" ? "#b3402f" : cell.color ?? text;
      const align =
        cell.align ?? (cell.kind === "num" ? "right" : cell.kind === "bool" ? "center" : "left");
      const tw = ctx.measureText(cell.text).width;
      const fits = tw <= cw - pad * 2;
      if (!fits) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(x, y, cw, rh);
        ctx.clip();
      }
      let tx = x + pad;
      ctx.textAlign = "left";
      if (align === "right") tx = x + cw - pad - tw;
      else if (align === "center") tx = x + (cw - tw) / 2;
      if (!fits && align !== "left") tx = Math.max(tx, x + pad);
      ctx.fillText(cell.text, tx, y + rh / 2 + 0.5);
      if (cell.underline) ctx.fillRect(tx, y + rh / 2 + 8, Math.min(tw, cw - pad), 1);
      if (!fits) ctx.restore();
    };

    if (d) {
      for (let r = r0; r <= r1; r++) {
        for (let c = c0; c <= c1; c++) {
          const key = (r + 1) * 100000 + (c + 1);
          const cell = d.cells.get(key);
          if (!cell || cell.text === "" || swallowed.has(key)) continue;
          drawText(cell, colScreen(c), rowScreen(r), colX[c + 1] - colX[c], rowY[r + 1] - rowY[r]);
        }
      }
      for (const m of visMerges) {
        const cell = d.cells.get(m.row * 100000 + m.col);
        if (!cell || cell.text === "") continue;
        const b = boxOf(m);
        drawText(cell, b.x, b.y, b.w, b.h);
      }
    }

    // selection (merged cells are always selected whole)
    const rect = expandRect(selRect(p.sel), merges);
    const sr0 = rect.row - 1;
    const sc0 = rect.col - 1;
    const sr1 = sr0 + rect.height;
    const sc1 = sc0 + rect.width;
    const selX = colScreen(sc0);
    const selY = rowScreen(sr0);
    const selW = colX[Math.min(sc1, cols)] - colX[sc0];
    const selH = rowY[Math.min(sr1, rows)] - rowY[sr0];
    ctx.fillStyle = selFill;
    ctx.fillRect(selX, selY, selW, selH);
    ctx.strokeStyle = accent;
    ctx.lineWidth = 2;
    ctx.strokeRect(selX + 1, selY + 1, selW - 2, selH - 2);
    if (rect.width * rect.height > 1 && (p.sel.ar !== p.sel.fr || p.sel.ac !== p.sel.fc || merges.length > 0)) {
      // the active cell is drawn un-tinted
      const active = anchorOf(merges, p.sel.ar, p.sel.ac);
      const a = boxOf(active);
      if (a.w < selW - 1 || a.h < selH - 1) {
        const cell = d?.cells.get(active.row * 100000 + active.col);
        ctx.fillStyle = cell?.fill ?? bg;
        ctx.fillRect(a.x + 1, a.y + 1, a.w - 2, a.h - 2);
        if (cell && cell.text !== "") {
          ctx.save();
          ctx.beginPath();
          ctx.rect(a.x + 1, a.y + 1, a.w - 2, a.h - 2);
          ctx.clip();
          drawText(cell, a.x, a.y, a.w, a.h);
          ctx.restore();
        }
        ctx.strokeStyle = accent;
        ctx.lineWidth = 2;
        ctx.strokeRect(a.x + 1, a.y + 1, a.w - 2, a.h - 2);
      }
    }
    for (const ref of p.refs) {
      if (ref.rect.row > rows || ref.rect.col > cols) continue;
      const pr = expandRect(ref.rect, merges);
      const px = colScreen(pr.col - 1);
      const py = rowScreen(pr.row - 1);
      const pw = colX[Math.min(pr.col - 1 + pr.width, cols)] - colX[pr.col - 1];
      const ph = rowY[Math.min(pr.row - 1 + pr.height, rows)] - rowY[pr.row - 1];
      ctx.fillStyle = `${ref.color}1a`;
      ctx.fillRect(px, py, pw, ph);
      ctx.strokeStyle = ref.color;
      ctx.lineWidth = 2;
      ctx.setLineDash([6, 3]);
      ctx.strokeRect(px + 1, py + 1, pw - 2, ph - 2);
      ctx.setLineDash([]);
    }
    // where the dragged cells would land
    if (moving) {
      const mx = colScreen(moving.col - 1);
      const my = rowScreen(moving.row - 1);
      const mw = colX[Math.min(moving.col - 1 + moving.width, cols)] - colX[moving.col - 1];
      const mh = rowY[Math.min(moving.row - 1 + moving.height, rows)] - rowY[moving.row - 1];
      ctx.fillStyle = selFill;
      ctx.fillRect(mx, my, mw, mh);
      ctx.strokeStyle = accent;
      ctx.lineWidth = 2;
      ctx.setLineDash([6, 3]);
      ctx.strokeRect(mx + 1, my + 1, mw - 2, mh - 2);
      ctx.setLineDash([]);
    }
    ctx.restore();

    // column headers
    ctx.fillStyle = headBg;
    ctx.fillRect(0, 0, w, HEADER_H);
    ctx.font = `500 11px ${SANS}`;
    ctx.textBaseline = "middle";
    ctx.textAlign = "center";
    for (let c = c0; c <= c1; c++) {
      const x = colScreen(c);
      const cw = colX[c + 1] - colX[c];
      const inSel = c >= sc0 && c < sc1;
      if (inSel) {
        ctx.fillStyle = headBgSel;
        ctx.fillRect(x, 0, cw, HEADER_H);
      }
      ctx.fillStyle = inSel ? headTextSel : headText;
      ctx.fillText(columnName(c + 1), x + cw / 2, HEADER_H / 2 + 1);
    }
    // row headers
    ctx.fillStyle = headBg;
    ctx.fillRect(0, HEADER_H, HEADER_W, h - HEADER_H);
    ctx.font = `400 11px ${MONO}`;
    for (let r = r0; r <= r1; r++) {
      const y = rowScreen(r);
      const rh = rowY[r + 1] - rowY[r];
      const inSel = r >= sr0 && r < sr1;
      if (inSel) {
        ctx.fillStyle = headBgSel;
        ctx.fillRect(0, y, HEADER_W, rh);
      }
      ctx.fillStyle = inSel ? headTextSel : headText;
      ctx.fillText(String(r + 1), HEADER_W / 2, y + rh / 2 + 1);
    }
    // header borders + corner
    ctx.strokeStyle = line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let c = c0; c <= c1 + 1 && c <= cols; c++) {
      const x = Math.round(colScreen(c)) - 0.5;
      ctx.moveTo(x, 0);
      ctx.lineTo(x, HEADER_H);
    }
    for (let r = r0; r <= r1 + 1 && r <= rows; r++) {
      const y = Math.round(rowScreen(r)) - 0.5;
      ctx.moveTo(0, y);
      ctx.lineTo(HEADER_W, y);
    }
    ctx.moveTo(0, HEADER_H - 0.5);
    ctx.lineTo(w, HEADER_H - 0.5);
    ctx.moveTo(HEADER_W - 0.5, 0);
    ctx.lineTo(HEADER_W - 0.5, h);
    ctx.stroke();
    ctx.fillStyle = headBg;
    ctx.fillRect(0, 0, HEADER_W - 1, HEADER_H - 1);

    // grab handle on the hovered / dragged column border
    const handleCol = resize?.col ?? hoverBorder;
    if (handleCol !== null) {
      const x = Math.round(colScreen(handleCol));
      if (x > HEADER_W) {
        ctx.fillStyle = accent;
        ctx.fillRect(x - 4, 7, 2, HEADER_H - 14);
        ctx.fillRect(x + 1, 7, 2, HEADER_H - 14);
        if (resize) ctx.fillRect(x - 0.5, HEADER_H, 1, h - HEADER_H);
      }
    }
  }, [size, scroll, vis, colX, rowY, rows, cols, p.sel, p.sheet, resize, hoverBorder, layout.merges, p.refs, moving]);

  useLayoutEffect(() => {
    draw();
  });

  // ---- mouse
  const hit = (clientX: number, clientY: number) => {
    const el = scroller.current!;
    const box = el.getBoundingClientRect();
    const x = clientX - box.left;
    const y = clientY - box.top;
    const onColHeader = y < HEADER_H;
    const onRowHeader = x < HEADER_W;
    const gx = Math.max(0, x - HEADER_W + el.scrollLeft);
    const gy = Math.max(0, y - HEADER_H + el.scrollTop);
    return {
      col: Math.min(cols, locate(colX, Math.min(gx, colX[cols] - 1)) + 1),
      row: Math.min(rows, locate(rowY, Math.min(gy, rowY[rows] - 1)) + 1),
      onColHeader,
      onRowHeader,
    };
  };

  /** Column (1-based) whose right border is under the pointer, header row only. */
  const borderAt = (clientX: number, clientY: number): number | null => {
    const el = scroller.current!;
    const box = el.getBoundingClientRect();
    const x = clientX - box.left;
    const y = clientY - box.top;
    if (y >= HEADER_H || x < HEADER_W - RESIZE_SLOP) return null;
    const gx = x - HEADER_W + el.scrollLeft;
    const c = locate(colX, Math.max(0, gx));
    if (colX[c + 1] - gx <= RESIZE_SLOP) return c + 1;
    if (c > 0 && gx - colX[c] <= RESIZE_SLOP) return c;
    return null;
  };

  /** Resizing a column inside a whole-column selection applies to all of them. */
  const targetColumns = (col: number): number[] => {
    const r = selRect(p.sel);
    const wholeCols = r.row === 1 && r.height === rows;
    if (wholeCols && col >= r.col && col < r.col + r.width) {
      return Array.from({ length: r.width }, (_, i) => r.col + i);
    }
    return [col];
  };

  /** The selection with the merged cells it touches whole. */
  const selectedRect = () => expandRect(selRect(p.sel), layout.merges);

  /** On the edge of the selection, where a drag carries the cells away. */
  const onSelectionEdge = (clientX: number, clientY: number): boolean => {
    if (p.editing) return false;
    const r = selectedRect();
    // whole rows and columns can't be moved this way
    if ((r.row === 1 && r.height >= rows) || (r.col === 1 && r.width >= cols)) return false;
    const el = scroller.current!;
    const box = el.getBoundingClientRect();
    const px = clientX - box.left;
    const py = clientY - box.top;
    if (px < HEADER_W || py < HEADER_H || px >= el.clientWidth || py >= el.clientHeight) return false;
    const x = px - HEADER_W + el.scrollLeft;
    const y = py - HEADER_H + el.scrollTop;
    const left = colX[r.col - 1];
    const right = colX[Math.min(r.col - 1 + r.width, cols)];
    const top = rowY[r.row - 1];
    const bottom = rowY[Math.min(r.row - 1 + r.height, rows)];
    const near =
      x >= left - GRAB_OUT && x <= right + GRAB_OUT && y >= top - GRAB_OUT && y <= bottom + GRAB_OUT;
    const inner =
      x > left + GRAB_IN && x < right - GRAB_IN && y > top + GRAB_IN && y < bottom - GRAB_IN;
    return near && !inner;
  };

  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    const el = scroller.current!;
    // clicks on the scrollbars must not move the selection
    if (e.clientX - el.getBoundingClientRect().left >= el.clientWidth) return;
    if (e.clientY - el.getBoundingClientRect().top >= el.clientHeight) return;
    if ((e.target as HTMLElement).tagName === "INPUT") return;
    e.preventDefault();
    const border = borderAt(e.clientX, e.clientY);
    if (p.editing && border === null && p.canPoint()) {
      const h = hit(e.clientX, e.clientY);
      if (!h.onColHeader && !h.onRowHeader) {
        // keep the editor focused: the click only feeds the formula
        dragging.current = "point";
        pointAnchor.current = { row: h.row, col: h.col };
        p.onPoint({ row: h.row, col: h.col, height: 1, width: 1 });
        return;
      }
    }
    el.focus();
    if (!e.shiftKey && onSelectionEdge(e.clientX, e.clientY)) {
      const h = hit(e.clientX, e.clientY);
      const from = selectedRect();
      dragging.current = "move";
      moveStart.current = { row: h.row, col: h.col, from };
      setMoving(from);
      setHoverEdge(false);
      return;
    }
    if (p.editing) p.commitEdit(0, 0);
    if (border !== null) {
      const width = colX[border] - colX[border - 1];
      dragging.current = "resize";
      resizeStart.current = { col: border, x: e.clientX, width };
      setResize({ col: border, width });
      return;
    }
    const h = hit(e.clientX, e.clientY);
    if (h.onColHeader && h.onRowHeader) {
      p.setSel({ ar: 1, ac: 1, fr: rows, fc: cols });
      return;
    }
    if (h.onColHeader) {
      dragging.current = "cols";
      p.setSel(
        e.shiftKey
          ? { ...p.sel, fr: rows, fc: h.col }
          : { ar: 1, ac: h.col, fr: rows, fc: h.col },
      );
    } else if (h.onRowHeader) {
      dragging.current = "rows";
      p.setSel(
        e.shiftKey
          ? { ...p.sel, fr: h.row, fc: cols }
          : { ar: h.row, ac: 1, fr: h.row, fc: cols },
      );
    } else {
      dragging.current = "cells";
      p.setSel(
        e.shiftKey
          ? { ...p.sel, fr: h.row, fc: h.col }
          : { ar: h.row, ac: h.col, fr: h.row, fc: h.col },
      );
    }
  };

  const onContextMenu = (e: React.MouseEvent) => {
    // the cell editor keeps the browser's own Cut / Copy / Paste menu
    if ((e.target as HTMLElement).tagName === "INPUT") return;
    e.preventDefault();
    const el = scroller.current!;
    const box = el.getBoundingClientRect();
    // not on the scrollbars
    if (e.clientX - box.left >= el.clientWidth || e.clientY - box.top >= el.clientHeight) return;
    // a ctrl-click on a Mac starts a drag that never sees its mouse-up
    dragging.current = null;
    // while a formula is being pointed at, the click is not for us
    if (p.editing && p.canPoint()) return;
    const h = hit(e.clientX, e.clientY);
    if (h.onColHeader && h.onRowHeader) return;
    el.focus();
    if (p.editing) p.commitEdit(0, 0);
    // like Sheets, a right-click inside the selection keeps it and one outside moves it
    const r = expandRect(selRect(p.sel), layout.merges);
    const inCols = h.col >= r.col && h.col < r.col + r.width;
    const inRows = h.row >= r.row && h.row < r.row + r.height;
    if (h.onColHeader) {
      if (!(inCols && r.row === 1 && r.height === rows)) {
        p.setSel({ ar: 1, ac: h.col, fr: rows, fc: h.col });
      }
    } else if (h.onRowHeader) {
      if (!(inRows && r.col === 1 && r.width === cols)) {
        p.setSel({ ar: h.row, ac: 1, fr: h.row, fc: cols });
      }
    } else if (!(inCols && inRows)) {
      p.setSel({ ar: h.row, ac: h.col, fr: h.row, fc: h.col });
    }
    p.onContextMenu({ left: e.clientX, top: e.clientY });
  };

  const resizeRef = useRef(resize);
  resizeRef.current = resize;
  const movingRef = useRef(moving);
  movingRef.current = moving;
  const selRef = useRef(p.sel);
  selRef.current = p.sel;
  useEffect(() => {
    const move = (e: MouseEvent) => {
      const mode = dragging.current;
      if (!mode) return;
      if (mode === "resize") {
        const start = resizeStart.current;
        const width = Math.round(
          Math.min(MAX_COL_W, Math.max(MIN_COL_W, start.width + e.clientX - start.x)),
        );
        setResize({ col: start.col, width });
        return;
      }
      const h = hit(e.clientX, e.clientY);
      if (mode === "move") {
        const { row, col, from } = moveStart.current;
        // the cells keep their shape and stay on the sheet
        setMoving({
          ...from,
          row: Math.min(Math.max(1, from.row + h.row - row), rows - from.height + 1),
          col: Math.min(Math.max(1, from.col + h.col - col), cols - from.width + 1),
        });
        return;
      }
      if (mode === "point") {
        const a = pointAnchor.current;
        p.onPoint({
          row: Math.min(a.row, h.row),
          col: Math.min(a.col, h.col),
          height: Math.abs(a.row - h.row) + 1,
          width: Math.abs(a.col - h.col) + 1,
        });
        return;
      }
      const s = selRef.current;
      if (mode === "cells") p.setSel({ ...s, fr: h.row, fc: h.col });
      else if (mode === "cols") p.setSel({ ...s, fr: rows, fc: h.col });
      else p.setSel({ ...s, fr: h.row, fc: cols });
    };
    const up = () => {
      if (dragging.current === "move") {
        const { from } = moveStart.current;
        const to = movingRef.current;
        setMoving(null);
        if (to && (to.row !== from.row || to.col !== from.col)) p.onMoveCells(from, to.row, to.col);
      }
      if (dragging.current === "resize") {
        const start = resizeStart.current;
        const width = resizeRef.current?.width ?? start.width;
        if (width !== start.width) p.onResizeColumns(targetColumns(start.col), width);
        else setResize(null);
      }
      dragging.current = null;
    };
    // Escape drops the cells where they were
    const cancel = (e: KeyboardEvent) => {
      if (e.key === "Escape" && dragging.current === "move") {
        dragging.current = null;
        setMoving(null);
      }
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    window.addEventListener("keydown", cancel);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      window.removeEventListener("keydown", cancel);
    };
  });

  const onDoubleClick = (e: React.MouseEvent) => {
    const border = borderAt(e.clientX, e.clientY);
    if (border !== null) {
      setResize(null);
      p.onAutofitColumns(targetColumns(border));
      return;
    }
    const h = hit(e.clientX, e.clientY);
    if (!h.onColHeader && !h.onRowHeader) p.startEdit();
  };

  // ---- cell editor
  const editorStyle = useMemo(() => {
    const m = anchorOf(layout.merges, p.sel.ar, p.sel.ac);
    const c = m.col - 1;
    const r = m.row - 1;
    const cEnd = Math.min(c + m.width, cols);
    const rEnd = Math.min(r + m.height, rows);
    return {
      left: HEADER_W + colX[c],
      top: HEADER_H + rowY[r],
      minWidth: colX[cEnd] - colX[c],
      height: rowY[rEnd] - rowY[r],
    };
  }, [p.sel.ac, p.sel.ar, colX, rowY, layout.merges, rows, cols]);

  const editor = useRef<HTMLInputElement>(null);
  // the editor starts at the cell's width and grows with its text, like Excel's
  const measure = useMemo(() => document.createElement("canvas").getContext("2d"), []);
  const editorWidth = (() => {
    if (!p.editing || !measure) return editorStyle.minWidth;
    measure.font = `13px ${SANS}`;
    const text = Math.ceil(measure.measureText(p.editing.value).width) + 7 * 2 + 2 * 2 + 4;
    const room = scroll.x + size.w - editorStyle.left;
    return Math.max(editorStyle.minWidth, Math.min(text, room));
  })();
  useEffect(() => {
    if (p.editing && document.activeElement?.id !== "formula-input") {
      const ed = editor.current;
      ed?.focus();
      // caret after the text, like Excel's F2
      ed?.setSelectionRange(ed.value.length, ed.value.length);
    }
  }, [p.editing !== null]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div
      ref={scroller}
      className={`grid-scroller ${hoverBorder !== null || resize ? "col-resize" : ""} ${moving ? "moving" : hoverEdge ? "can-move" : ""}`}
      tabIndex={0}
      data-grid="true"
      onScroll={(e) => {
        const t = e.currentTarget;
        setScroll({ x: t.scrollLeft, y: t.scrollTop });
      }}
      onMouseDown={onMouseDown}
      onContextMenu={onContextMenu}
      onMouseMove={(e) => {
        if (!dragging.current) {
          const edge = onSelectionEdge(e.clientX, e.clientY);
          if (edge !== hoverEdge) setHoverEdge(edge);
        }
        if (dragging.current && dragging.current !== "resize") return;
        const border = dragging.current ? resizeStart.current.col : borderAt(e.clientX, e.clientY);
        if (border !== hoverBorder) setHoverBorder(border);
      }}
      onMouseLeave={() => {
        setHoverEdge(false);
        if (!dragging.current) setHoverBorder(null);
      }}
      onDoubleClick={onDoubleClick}
      onKeyDown={p.onKeyDown}
    >
      <div style={{ width: totalW, height: totalH, position: "relative" }}>
        <canvas
          ref={canvas}
          style={{
            position: "sticky",
            top: 0,
            left: 0,
            display: "block",
            width: size.w,
            height: size.h,
          }}
        />
        {p.editing && p.preview && (
          <div
            className={`formula-preview ${p.preview.kind === "err" ? "err" : ""}`}
            style={{
              left: editorStyle.left,
              // above the cell, or below it when the column headers would cover it
              top:
                editorStyle.top - PREVIEW_H - 4 < scroll.y + HEADER_H
                  ? editorStyle.top + editorStyle.height + 4
                  : editorStyle.top - PREVIEW_H - 4,
              height: PREVIEW_H,
            }}
          >
            {p.preview.text}
          </div>
        )}
        {p.editing && (
          <div className="cell-editor-box" style={{ left: editorStyle.left, top: editorStyle.top }}>
            <input
              ref={editor}
              className={`cell-editor ${p.refs.length ? "tinted" : ""}`}
              style={{ width: editorWidth, height: editorStyle.height }}
              value={p.editing.value}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              onChange={(e) => p.setEditValue(e.target.value)}
              onMouseDown={(e) => e.stopPropagation()}
              onKeyDown={(e) => {
                if (e.nativeEvent.isComposing) return;
                if (e.key === "Enter") {
                  e.preventDefault();
                  p.commitEdit(e.shiftKey ? -1 : 1, 0);
                } else if (e.key === "Tab") {
                  e.preventDefault();
                  p.commitEdit(0, e.shiftKey ? -1 : 1);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  p.cancelEdit();
                } else if (e.key in ARROWS && !e.metaKey && !e.altKey) {
                  const [dr, dc] = ARROWS[e.key];
                  if (p.onPointKey(dr, dc, e.shiftKey)) e.preventDefault();
                }
                e.stopPropagation();
              }}
            />
            {p.refs.length > 0 && <FormulaMirror value={p.editing.value} refs={p.refs} input={editor} />}
          </div>
        )}
      </div>
    </div>
  );
}
