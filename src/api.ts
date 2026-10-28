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
  newFile: () => invoke<WorkbookInfo>("new_file"),
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
  autosumRange: (sheet: number, row: number, col: number) =>
    invoke<Rect | null>("autosum_range", { sheet, row, col }),
  previewFormula: (sheet: number, row: number, col: number, formula: string) =>
    invoke<FormulaPreview>("preview_formula", { sheet, row, col, formula }),
  getActiveStyle: (sheet: number, row: number, col: number) =>
    invoke<ActiveStyle>("get_active_style", { sheet, row, col }),
  getRangeText: (sheet: number, r: Rect, raw: boolean) =>
    invoke<string[][]>("get_range_text", { sheet, ...rectArgs(r), raw }),
  rangeStats: (sheet: number, r: Rect) =>
    invoke<RangeStats>("range_stats", { sheet, ...rectArgs(r) }),
  undo: () => invoke<WorkbookInfo>("undo"),
  redo: () => invoke<WorkbookInfo>("redo"),
  addSheet: () => invoke<WorkbookInfo>("add_sheet"),
  saveFile: (path: string | null, sheet: number) =>
    invoke<WorkbookInfo>("save_file", { path, sheet }),
  takePendingOpen: () => invoke<string[]>("take_pending_open"),
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
