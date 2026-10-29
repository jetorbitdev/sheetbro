import { invoke } from "@tauri-apps/api/core";

export interface SheetInfo {
  name: string;
  rows: number;
  cols: number;
}

export interface WorkbookInfo {
  name: string;
  path: string | null;
  dirty: boolean;
  sheets: SheetInfo[];
  can_undo: boolean;
  can_redo: boolean;
  /** untouched empty workbook: a file opened now replaces it instead of getting a new window */
  blank: boolean;
}

/** 1-based anchor plus size of a merged range */
export interface Merge {
  row: number;
  col: number;
  height: number;
  width: number;
}

export interface Layout {
  merges: Merge[];
  col_widths: number[];
  default_row_height: number;
  row_heights: [number, number][];
}

export interface CellData {
  r: number;
  c: number;
  text: string;
  kind: "num" | "text" | "bool" | "err";
  bold: boolean;
  italic: boolean;
  underline: boolean;
  color: string | null;
  fill: string | null;
  align: "left" | "center" | "right" | null;
}

export interface ActiveStyle {
  bold: boolean;
  italic: boolean;
  underline: boolean;
  num_fmt: string;
}

export interface RangeStats {
  count: number;
  numeric: number;
  sum: number;
}

/** 1-based, inclusive rectangle */
export interface Rect {
  row: number;
  col: number;
  height: number;
  width: number;
}

export type MergeMode = "merge" | "center" | "across" | "down";

export type Axis = "rows" | "columns";

/** which way the cells at and after an insertion point move */
export type Shift = "down" | "right";

/** which way the cells after deleted ones move */
export type Pull = "up" | "left";

/** What a formula being typed would show, before it is entered */
export interface FormulaPreview {
  text: string;
  kind: CellData["kind"];
}

/** State of the MCP server that lets AI tools edit the open workbook */
export interface McpStatus {
  enabled: boolean;
  running: boolean;
  port: number;
  url: string;
  /** secret the HTTP clients send as a bearer token */
  token: string;
  /** this app's executable; stdio-only clients run it with `--mcp-stdio` */
  command: string;
  error: string | null;
}

/** payload of the `workbook-changed` event: an AI tool changed the workbook */
export interface WorkbookChange {
  /** a different file was loaded, so the view must start over */
  replaced: boolean;
  info: WorkbookInfo;
}

const rectArgs = (r: Rect) => r;

export const api = {
  openFile: (path: string) => invoke<WorkbookInfo>("open_file", { path }),
  workbookInfo: () => invoke<WorkbookInfo>("workbook_info"),
  getLayout: (sheet: number) => invoke<Layout>("get_layout", { sheet }),
  getViewport: (
    sheet: number,
    rowStart: number,
    rowEnd: number,
    colStart: number,
    colEnd: number,
  ) =>
    invoke<CellData[]>("get_viewport", { sheet, rowStart, rowEnd, colStart, colEnd }),
  getCellContent: (sheet: number, row: number, col: number) =>
    invoke<string>("get_cell_content", { sheet, row, col }),
  setCells: (sheet: number, row: number, col: number, values: string[][]) =>
    invoke<WorkbookInfo>("set_cells", { sheet, row, col, values }),
  clearRange: (sheet: number, r: Rect) =>
    invoke<WorkbookInfo>("clear_range", { sheet, ...rectArgs(r) }),
  styleRange: (sheet: number, r: Rect, path: string, value: string) =>
    invoke<WorkbookInfo>("style_range", { sheet, ...rectArgs(r), path, value }),
  setColumnWidths: (sheet: number, widths: [number, number][]) =>
    invoke<WorkbookInfo>("set_column_widths", { sheet, widths }),
  columnTexts: (sheet: number, col: number) =>
    invoke<{ text: string; bold: boolean; num: boolean }[]>("column_texts", { sheet, col }),
  mergeCells: (sheet: number, r: Rect, mode: MergeMode, discard: boolean) =>
    invoke<WorkbookInfo>("merge_cells", { sheet, ...rectArgs(r), mode, discard }),
  unmergeCells: (sheet: number, r: Rect) =>
    invoke<WorkbookInfo>("unmerge_cells", { sheet, ...rectArgs(r) }),
  /** `at` is the 1-based row or column the new ones go before */
  insertLines: (sheet: number, axis: Axis, at: number, count: number) =>
    invoke<WorkbookInfo>("insert_lines", { sheet, axis, at, count }),
  deleteLines: (sheet: number, axis: Axis, at: number, count: number) =>
    invoke<WorkbookInfo>("delete_lines", { sheet, axis, at, count }),
  /** inserts empty cells over the range and pushes the cells after it down or to the right */
  insertCells: (sheet: number, r: Rect, shift: Shift) =>
    invoke<WorkbookInfo>("insert_cells", { sheet, ...rectArgs(r), shift }),
  /** deletes the cells of the range and pulls the cells after it up or to the left */
  deleteCells: (sheet: number, r: Rect, pull: Pull) =>
    invoke<WorkbookInfo>("delete_cells", { sheet, ...rectArgs(r), pull }),
  /** moves the cells of the range, with their formatting, so that its top-left corner is at (row, col) */
  moveCells: (sheet: number, r: Rect, row: number, col: number) =>
    invoke<WorkbookInfo>("move_cells", { sheet, ...rectArgs(r), toRow: row, toCol: col }),
  /** remembers the formatting of the range for `pasteFormats` */
  copyFormats: (sheet: number, r: Rect) =>
    invoke<void>("copy_formats", { sheet, ...rectArgs(r) }),
  pasteFormats: (sheet: number, r: Rect) =>
    invoke<WorkbookInfo>("paste_formats", { sheet, ...rectArgs(r) }),
  autosumRange: (sheet: number, row: number, col: number) =>
    invoke<Rect | null>("autosum_range", { sheet, row, col }),
  previewFormula: (sheet: number, row: number, col: number, formula: string) =>
    invoke<FormulaPreview>("preview_formula", { sheet, row, col, formula }),
  getActiveStyle: (sheet: number, row: number, col: number) =>
    invoke<ActiveStyle>("get_active_style", { sheet, row, col }),
  getRangeText: (sheet: number, r: Rect, raw: boolean) =>
    invoke<string[][]>("get_range_text", { sheet, ...rectArgs(r), raw }),
  /** computed values only: no formulas, no number formats */
  getRangeValues: (sheet: number, r: Rect) =>
    invoke<string[][]>("get_range_values", { sheet, ...rectArgs(r) }),
  rangeStats: (sheet: number, r: Rect) =>
    invoke<RangeStats>("range_stats", { sheet, ...rectArgs(r) }),
  undo: () => invoke<WorkbookInfo>("undo"),
  redo: () => invoke<WorkbookInfo>("redo"),
  addSheet: () => invoke<WorkbookInfo>("add_sheet"),
  saveFile: (path: string | null, sheet: number) =>
    invoke<WorkbookInfo>("save_file", { path, sheet }),
  takePendingOpen: () => invoke<string[]>("take_pending_open"),
  /**
   * Opens another window, empty or showing `path`, or focuses the window that has `path` open.
   * With `here`, true means this window should load `path` itself.
   */
  openInWindow: (path: string | null, here = false) =>
    invoke<boolean>("open_in_window", { path, here }),
  mcpStatus: () => invoke<McpStatus>("mcp_status"),
  mcpSetEnabled: (enabled: boolean, port?: number) =>
    invoke<McpStatus>("mcp_set_enabled", { enabled, port }),
  mcpRegenerateToken: () => invoke<McpStatus>("mcp_regenerate_token"),
};

export function columnName(col: number): string {
  let s = "";
  for (let n = col; n > 0; n = Math.floor((n - 1) / 26)) {
    s = String.fromCharCode(65 + ((n - 1) % 26)) + s;
  }
  return s;
}

export function cellName(row: number, col: number): string {
  return `${columnName(col)}${row}`;
}

/** "B12" -> {row: 12, col: 2}; null when not a plain A1 reference */
export function parseCellName(text: string): { row: number; col: number } | null {
  const m = /^\$?([A-Za-z]{1,3})\$?(\d{1,7})$/.exec(text.trim());
  if (!m) return null;
  let col = 0;
  for (const ch of m[1].toUpperCase()) col = col * 26 + ch.charCodeAt(0) - 64;
  return { row: Number(m[2]), col };
}
