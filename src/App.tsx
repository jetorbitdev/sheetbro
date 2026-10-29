import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import {
  ActiveStyle,
  api,
  cellName,
  FormulaPreview,
  Layout,
  McpStatus,
  MergeMode,
  parseCellName,
  RangeStats,
  Rect,
  WorkbookChange,
  WorkbookInfo,
} from "./api";
import { Grid, MONO, SANS, Sel, selRect } from "./Grid";
import {
  AlignCenterIcon,
  AlignLeftIcon,
  AlignRightIcon,
  AppMark,
  FillColorIcon,
  McpIcon,
  MergeIcon,
  NoFillIcon,
  PlusIcon,
  RedoIcon,
  TextColorIcon,
  UndoIcon,
} from "./Icons";
import { anchorOf, expandRect } from "./merges";
import { FunctionMenu } from "./FunctionMenu";
import { McpPanel } from "./McpPanel";
import { FormulaMirror } from "./FormulaMirror";
import { docFor, expectsReference, formulaRefs, functionContext, normalizeFormula, parseSyntax } from "./formula";
import { adjustDecimals, parseTsv, toTsv } from "./tsv";

const MAX_ROWS = 1_048_576;
const A1: Sel = { ar: 1, ac: 1, fr: 1, fc: 1 };
const EMPTY_LAYOUT: Layout = {
  merges: [],
  col_widths: Array(26).fill(90),
  default_row_height: 25,
  row_heights: [],
};
const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));
const errText = (e: unknown) => (typeof e === "string" ? e : (e as Error)?.message ?? String(e));

export function App() {
  const [info, setInfo] = useState<WorkbookInfo | null>(null);
  const [sheet, setSheet] = useState(0);
  const [docId, setDocId] = useState(0);
  const [layout, setLayout] = useState<Layout>(EMPTY_LAYOUT);
  const [sel, setSel] = useState<Sel>(A1);
  const [rev, setRev] = useState(0);
  const [editing, setEditing] = useState<{ value: string } | null>(null);
  const [content, setContent] = useState("");
  const [style, setStyle] = useState<ActiveStyle | null>(null);
  const [stats, setStats] = useState<RangeStats | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [blank, setBlank] = useState(true);
  const [dropping, setDropping] = useState(false);
  const [nameDraft, setNameDraft] = useState<string | null>(null);
  const clipboard = useRef<{ text: string; raw: string[][] } | null>(null);
  const [fnMenu, setFnMenu] = useState<{ left: number; top: number } | null>(null);
  const [mcp, setMcp] = useState<McpStatus | null>(null);
  const [mcpOpen, setMcpOpen] = useState(false);
  // last cell reference inserted by clicking (or arrowing) while typing a formula,
  // with the cells it covers so the arrow keys can carry on from there
  const pointRefRef = useRef<{ start: number; end: number; cursor: Sel } | null>(null);
  const [pointFocus, setPointFocus] = useState<{ row: number; col: number } | null>(null);
  const [preview, setPreview] = useState<FormulaPreview | null>(null);
  const formulaInput = useRef<HTMLInputElement>(null);
  const editingRef = useRef(editing);
  editingRef.current = editing;
  const caretRequest = useRef<number | null>(null);

  // ---------------------------------------------------------------- helpers
  const showError = useCallback((e: unknown) => {
    setToast(errText(e));
  }, []);
  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const applyInfo = useCallback((i: WorkbookInfo) => {
    setInfo(i);
    setRev((r) => r + 1);
  }, []);

  const mutate = useCallback(
    async (p: Promise<WorkbookInfo>) => {
      try {
        applyInfo(await p);
      } catch (e) {
        showError(e);
      }
    },
    [applyInfo, showError],
  );

  const focusGrid = () =>
    (document.querySelector("[data-grid]") as HTMLElement | null)?.focus();

  const rows = clamp((info?.sheets[sheet]?.rows ?? 1) + 100, 1000, MAX_ROWS);
  const cols = Math.max(layout.col_widths.length, (info?.sheets[sheet]?.cols ?? 1) + 10);
  const merges = layout.merges;
  // selecting part of a merged cell selects all of it
  const rect = useMemo(() => expandRect(selRect(sel), merges), [sel, merges]);
  // the active cell of a merge is its top-left cell
  const active = useMemo(() => anchorOf(merges, sel.ar, sel.ac), [sel.ar, sel.ac, merges]);

  // ------------------------------------------------------------ doc loading
  useEffect(() => {
    api.workbookInfo().then(setInfo).catch(showError);
  }, [showError]);

  useEffect(() => {
    api.mcpStatus().then(setMcp).catch(() => {});
  }, []);

  useEffect(() => {
    api.getLayout(sheet).then(setLayout).catch(showError);
    // rev: column widths can change through edits and undo/redo
  }, [sheet, docId, rev, showError]);

  const resetView = (sheetIndex = 0) => {
    setSheet(sheetIndex);
    setSel(A1);
    setEditing(null);
    setDocId((d) => d + 1);
  };

  const confirmDiscard = async () => {
    if (!info?.dirty) return true;
    return ask("This workbook has unsaved changes. Discard them?", {
      title: "Unsaved changes",
      kind: "warning",
      okLabel: "Discard",
      cancelLabel: "Cancel",
    });
  };

  const openPath = async (path: string) => {
    if (!(await confirmDiscard())) return;
    try {
      const i = await api.openFile(path);
      resetView();
      setBlank(false);
      applyInfo(i);
      focusGrid();
    } catch (e) {
      showError(e);
    }
  };

  /** The first file replaces an untouched empty workbook; the others get windows of their own. */
  const openPaths = async (paths: string[]) => {
    for (const [i, path] of paths.entries()) {
      try {
        const here = await api.openInWindow(path, i === 0 && !!info?.blank);
        if (here) await openPath(path);
      } catch (e) {
        showError(e);
      }
    }
  };

  const doOpen = async () => {
    const picked = await open({
      multiple: true,
      filters: [{ name: "Spreadsheets", extensions: ["xlsx", "xlsm", "csv", "tsv", "txt"] }],
    });
    if (picked) await openPaths(picked);
  };

  const doSaveAs = async () => {
    if (!info) return;
    let target = await save({
      defaultPath: `${info.name}.xlsx`,
      filters: [
        { name: "Excel Workbook", extensions: ["xlsx"] },
        { name: "CSV (current sheet)", extensions: ["csv"] },
      ],
    });
    if (!target) return;
    if (!/\.(xlsx|csv)$/i.test(target)) target += ".xlsx";
    try {
      applyInfo(await api.saveFile(target, sheet));
      if (/\.csv$/i.test(target)) setToast("Exported current sheet to CSV");
    } catch (e) {
      showError(e);
    }
  };

  const doSave = async () => {
    if (!info) return;
    if (!info.path) return doSaveAs();
    try {
      applyInfo(await api.saveFile(null, sheet));
    } catch (e) {
      showError(e);
    }
  };

  // ---------------------------------------------------------------- editing
  const startEdit = async (initial?: string) => {
    if (initial !== undefined) {
      setEditing({ value: initial });
      return;
    }
    try {
      setEditing({ value: await api.getCellContent(sheet, active.row, active.col) });
    } catch (e) {
      showError(e);
    }
  };

  /** Moves the active cell, hopping over a merged cell as a whole. */
  const moveActive = (dr: number, dc: number) => {
    const baseR = dr > 0 ? active.row + active.height - 1 : active.row;
    const baseC = dc > 0 ? active.col + active.width - 1 : active.col;
    const t = anchorOf(merges, clamp(baseR + dr, 1, rows), clamp(baseC + dc, 1, cols));
    setSel({ ar: t.row, ac: t.col, fr: t.row, fc: t.col });
  };

  const commitEdit = (dr: number, dc: number) => {
    if (!editing) return;
    const value = normalizeFormula(editing.value);
    const { row: ar, col: ac } = active;
    setEditing(null);
    moveActive(dr, dc);
    focusGrid();
    if (value !== content) {
      setBlank(false);
      mutate(api.setCells(sheet, ar, ac, [[value]]));
    }
  };

  const cancelEdit = () => {
    setEditing(null);
    focusGrid();
  };

  // ------------------------------------------------------- formula editing
  /** The input the user is typing in: the cell editor or the formula bar. */
  const editorInput = (): HTMLInputElement | null => {
    const el = document.activeElement;
    if (el instanceof HTMLInputElement && (el.id === "formula-input" || el.classList.contains("cell-editor"))) {
      return el;
    }
    return document.querySelector<HTMLInputElement>(".cell-editor");
  };

  /** Typing replaces whatever reference a click inserted. */
  const changeEdit = (value: string) => {
    pointRefRef.current = null;
    setPointFocus(null);
    editingRef.current = { value };
    setEditing({ value });
  };

  useEffect(() => {
    if (editing) return;
    pointRefRef.current = null;
    setPointFocus(null);
  }, [editing]);

  // React resets the caret when it rewrites an input's value; put it back where we want it
  useLayoutEffect(() => {
    const at = caretRequest.current;
    if (at === null) return;
    caretRequest.current = null;
    const el = editorInput();
    el?.focus();
    el?.setSelectionRange(at, at);
  });

  const canPoint = () => {
    const ed = editingRef.current;
    if (!ed) return false;
    const caret = editorInput()?.selectionStart ?? ed.value.length;
    const last = pointRefRef.current;
    return (last !== null && last.end === caret) || expectsReference(ed.value, caret);
  };

  const rangeName = (r: Rect) =>
    r.width * r.height > 1
      ? `${cellName(r.row, r.col)}:${cellName(r.row + r.height - 1, r.col + r.width - 1)}`
      : cellName(r.row, r.col);

  /** A click or drag on cells while typing a formula inserts (or replaces) a reference. */
  const onPoint = (
    r: Rect,
    cursor: Sel = { ar: r.row, ac: r.col, fr: r.row + r.height - 1, fc: r.col + r.width - 1 },
  ) => {
    const ed = editingRef.current;
    if (!ed) return;
    const target = expandRect(r, merges);
    const text = rangeName(target);
    const caret = editorInput()?.selectionStart ?? ed.value.length;
    const last = pointRefRef.current;
    const replace = last !== null && last.end === caret;
    const start = replace ? last.start : caret;
    const end = replace ? last.end : caret;
    const value = ed.value.slice(0, start) + text + ed.value.slice(end);
    const next = { start, end: start + text.length, cursor };
    editingRef.current = { value };
    pointRefRef.current = next;
    caretRequest.current = next.end;
    setEditing({ value });
  };

  /**
   * Arrow keys right after `=`, `(`, `,`, an operator, or a reference they
   * picked: walk the reference from the cell being edited (Shift stretches it
   * into a range), like Excel. Anywhere else they move the caret.
   */
  const onPointKey = (dr: number, dc: number, extend: boolean) => {
    if (!canPoint()) return false;
    const caret = editorInput()?.selectionStart ?? editingRef.current?.value.length ?? 0;
    const last = pointRefRef.current;
    const picked = last !== null && last.end === caret;
    let cursor: Sel;
    if (picked && extend) {
      const c = last.cursor;
      cursor = { ...c, fr: clamp(c.fr + dr, 1, rows), fc: clamp(c.fc + dc, 1, cols) };
    } else {
      // step off the far edge of a merged cell, so one press leaves it
      const m = picked ? anchorOf(merges, last.cursor.fr, last.cursor.fc) : active;
      const r = (dr > 0 ? m.row + m.height - 1 : m.row) + dr;
      const c = (dc > 0 ? m.col + m.width - 1 : m.col) + dc;
      const t = anchorOf(merges, clamp(r, 1, rows), clamp(c, 1, cols));
      cursor = { ar: t.row, ac: t.col, fr: t.row, fc: t.col };
    }
    onPoint(selRect(cursor), cursor);
    setPointFocus({ row: cursor.fr, col: cursor.fc });
    return true;
  };

  /** Starts (or continues) a formula with `NAME(` at the caret. */
  const insertFunction = (name: string) => {
    const ed = editingRef.current;
    if (!ed) {
      setEditing({ value: `=${name}(` });
      return;
    }
    const plain = !ed.value.startsWith("=");
    const base = plain ? "=" : ed.value;
    const caret = plain ? 1 : editorInput()?.selectionStart ?? ed.value.length;
    const value = `${base.slice(0, caret)}${name}(${base.slice(caret)}`;
    pointRefRef.current = null;
    editingRef.current = { value };
    caretRequest.current = caret + name.length + 1;
    setEditing({ value });
  };

  /** SUM & co. from the menu: totals the data next to the selection like Excel's AutoSum. */
  const autoFunction = async (fn: string) => {
    if (editing) return insertFunction(fn);
    try {
      let target: Rect;
      let formulas: string[];
      if (rect.width * rect.height > 1) {
        if (rect.height === 1) {
          // one row: total goes to its right
          target = { row: rect.row, col: rect.col + rect.width, height: 1, width: 1 };
          formulas = [`=${fn}(${rangeName(rect)})`];
        } else {
          // block: one total under each column
          target = { row: rect.row + rect.height, col: rect.col, height: 1, width: rect.width };
          formulas = Array.from(
            { length: rect.width },
            (_, i) => `=${fn}(${rangeName({ ...rect, col: rect.col + i, width: 1 })})`,
          );
        }
      } else {
        const found = content === "" ? await api.autosumRange(sheet, active.row, active.col) : null;
        if (!found) return void startEdit(`=${fn}(`);
        target = { row: active.row, col: active.col, height: 1, width: 1 };
        formulas = [`=${fn}(${rangeName(found)})`];
      }
      setBlank(false);
      await mutate(api.setCells(sheet, target.row, target.col, [formulas]));
      setSel({ ar: target.row, ac: target.col, fr: target.row, fc: target.col + formulas.length - 1 });
      focusGrid();
    } catch (e) {
      showError(e);
    }
  };

  const pickFunction = (name: string, quick: boolean) => {
    setFnMenu(null);
    if (quick) void autoFunction(name);
    else insertFunction(name);
  };

  // references in the formula being typed, colored in the text and boxed on the grid
  const refs = useMemo(() => (editing ? formulaRefs(editing.value) : []), [editing?.value]); // eslint-disable-line react-hooks/exhaustive-deps

  // what the formula would show, worked out while it is typed
  const editValue = editing?.value;
  useEffect(() => {
    if (!editValue || !editValue.startsWith("=") || editValue.length < 2) {
      setPreview(null);
      return;
    }
    let live = true;
    const t = setTimeout(() => {
      api
        .previewFormula(sheet, active.row, active.col, normalizeFormula(editValue))
        // #ERROR! means it doesn't parse yet; say nothing until it does
        .then((p) => live && setPreview(p.text === "" || p.text === "#ERROR!" ? null : p))
        .catch(() => live && setPreview(null));
    }, 120);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [editValue, sheet, active.row, active.col]);

  // argument hint while typing inside a function call
  const hintCtx = editing ? functionContext(editing.value, editorInput()?.selectionStart ?? editing.value.length) : null;
  const hintDoc = hintCtx ? docFor(hintCtx.name) : undefined;

  // active cell content / style for the formula bar and toolbar
  useEffect(() => {
    let live = true;
    api.getCellContent(sheet, active.row, active.col).then((c) => live && setContent(c)).catch(() => {});
    api.getActiveStyle(sheet, active.row, active.col).then((s) => live && setStyle(s)).catch(() => {});
    return () => {
      live = false;
    };
  }, [sheet, active.row, active.col, rev]);

  // Sum / Average / Count of the selection
  useEffect(() => {
    if (rect.width * rect.height <= 1) {
      setStats(null);
      return;
    }
    let live = true;
    const t = setTimeout(() => {
      api.rangeStats(sheet, rect).then((s) => live && setStats(s)).catch(() => {});
    }, 80);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [sheet, rect, rev]);

  // ------------------------------------------------------------- clipboard
  const copy = async (cut: boolean) => {
    try {
      const [vals, raw] = await Promise.all([
        api.getRangeText(sheet, rect, false),
        api.getRangeText(sheet, rect, true),
      ]);
      const text = toTsv(vals);
      clipboard.current = { text, raw };
      await writeText(text);
      if (cut) await mutate(api.clearRange(sheet, rect));
    } catch (e) {
      showError(e);
    }
  };

  const paste = async () => {
    try {
      const text = await readText();
      if (!text) return;
      // Our own clipboard keeps formulas; foreign text is plain values.
      const own = clipboard.current?.text === text;
      let values = own ? clipboard.current!.raw : parseTsv(text);
      let { row, col } = rect;
      let h = values.length;
      let w = Math.max(...values.map((r) => r.length));
      if (h === 1 && w === 1 && rect.height * rect.width > 1 && rect.height * rect.width <= 10000) {
        // single value into a larger selection fills it
        const v = values[0][0];
        values = Array.from({ length: rect.height }, () => Array(rect.width).fill(v));
        h = rect.height;
        w = rect.width;
      }
      h = Math.min(h, rows - row + 1);
      w = Math.min(w, cols - col + 1);
      setBlank(false);
      await mutate(api.setCells(sheet, row, col, values.slice(0, h).map((r) => r.slice(0, w))));
      setSel({ ar: row, ac: col, fr: row + h - 1, fc: col + w - 1 });
    } catch (e) {
      showError(e);
    }
  };

  const clearSelection = () => mutate(api.clearRange(sheet, rect));

  // ---------------------------------------------------------------- styling
  const applyStyle = (path: string, value: string) =>
    mutate(api.styleRange(sheet, rect, path, value));
  const toggleStyle = (key: "bold" | "italic" | "underline") => {
    const path = { bold: "font.b", italic: "font.i", underline: "font.u" }[key];
    applyStyle(path, String(!style?.[key]));
  };

  // ------------------------------------------------------------ merge cells
  const touchesMerge = merges.some(
    (m) =>
      m.row < rect.row + rect.height &&
      m.row + m.height > rect.row &&
      m.col < rect.col + rect.width &&
      m.col + m.width > rect.col,
  );

  const doMerge = async (mode: MergeMode) => {
    const tooSmall =
      mode === "across" ? rect.width < 2 : mode === "down" ? rect.height < 2 : rect.width * rect.height < 2;
    if (tooSmall) {
      setToast("Select more than one cell to merge");
      return;
    }
    try {
      applyInfo(await api.mergeCells(sheet, rect, mode, false));
    } catch (e) {
      if (errText(e) !== "merge_conflict") return showError(e);
      // Excel keeps only the upper-left value; make the user confirm that
      const ok = await ask(
        "Merging cells keeps only the upper-left value and discards the other values.",
        { title: "Merge cells", kind: "warning", okLabel: "Merge", cancelLabel: "Cancel" },
      );
      if (ok) await mutate(api.mergeCells(sheet, rect, mode, true));
    }
  };

  const unmerge = () => mutate(api.unmergeCells(sheet, rect));
  const toggleMerge = () => (touchesMerge ? unmerge() : doMerge("center"));

  // --------------------------------------------------------- column widths
  const resizeColumns = (colsToSet: number[], width: number) =>
    mutate(api.setColumnWidths(sheet, colsToSet.map((c) => [c, width])));

  const autofitColumns = async (colsToFit: number[]) => {
    try {
      const ctx = document.createElement("canvas").getContext("2d")!;
      const widths: [number, number][] = [];
      for (const c of colsToFit) {
        let max = 0;
        for (const t of await api.columnTexts(sheet, c)) {
          // same fonts the grid draws with, plus 8px padding each side
          ctx.font = `${t.bold ? 600 : 400} 13px ${t.num ? MONO : SANS}`;
          max = Math.max(max, ctx.measureText(t.text).width);
        }
        widths.push([c, max === 0 ? 90 : clamp(Math.ceil(max + 18), 24, 1000)]);
      }
      await mutate(api.setColumnWidths(sheet, widths));
    } catch (e) {
      showError(e);
    }
  };

  // -------------------------------------------------------------- shortcuts
  const handleCommand = async (id: string) => {
    const el = document.activeElement;
    const inText = el instanceof HTMLInputElement;
    if (inText && ["undo", "redo", "cut", "copy", "paste", "select_all"].includes(id)) {
      // native text editing inside the cell editor / formula bar
      if (id === "paste") {
        const t = await readText();
        if (t) document.execCommand("insertText", false, t);
      } else {
        document.execCommand({ select_all: "selectAll" }[id] ?? id);
      }
      return;
    }
    if (editing && id.startsWith("merge")) return;
    switch (id) {
      case "new": return api.openInWindow(null).catch(showError);
      case "open": return doOpen();
      case "save": return doSave();
      case "save_as": return doSaveAs();
      case "undo": return mutate(api.undo());
      case "redo": return mutate(api.redo());
      case "cut": return copy(true);
      case "copy": return copy(false);
      case "paste": return paste();
      case "select_all": return setSel({ ar: 1, ac: 1, fr: rows, fc: cols });
      case "bold": return toggleStyle("bold");
      case "italic": return toggleStyle("italic");
      case "underline": return toggleStyle("underline");
      case "merge_center": return doMerge("center");
      case "merge": return doMerge("merge");
      case "merge_across": return doMerge("across");
      case "merge_down": return doMerge("down");
      case "unmerge": return unmerge();
    }
  };
  const commandRef = useRef(handleCommand);
  commandRef.current = handleCommand;
  const openRef = useRef(openPath);
  openRef.current = openPath;
  const openPathsRef = useRef(openPaths);
  openPathsRef.current = openPaths;
  const dirtyRef = useRef(false);
  dirtyRef.current = info?.dirty ?? false;

  // an AI tool changed the workbook through MCP
  const onWorkbookChanged = ({ replaced, info: next }: WorkbookChange) => {
    setBlank(false);
    // a different file, or the sheet we were looking at no longer exists (undone "add sheet")
    if (replaced || sheet >= next.sheets.length) resetView();
    applyInfo(next);
    setToast(replaced ? `An AI tool opened ${next.name}` : "Updated by an AI tool");
  };
  const changedRef = useRef(onWorkbookChanged);
  changedRef.current = onWorkbookChanged;

  useEffect(() => {
    // only events sent to this window: every window has its own workbook
    const win = getCurrentWindow();
    const unlisten: Promise<() => void>[] = [
      win.listen<string>("menu", (e) => commandRef.current(e.payload)),
      win.listen<string>("open-file", (e) => openRef.current(e.payload)),
      win.listen<WorkbookChange>("workbook-changed", (e) => changedRef.current(e.payload)),
      getCurrentWebview().onDragDropEvent((e) => {
        const t = e.payload.type;
        if (t === "enter" || t === "over") setDropping(true);
        else if (t === "leave") setDropping(false);
        else if (t === "drop") {
          setDropping(false);
          void openPathsRef.current(e.payload.paths);
        }
      }),
      win.onCloseRequested(async (e) => {
        if (!dirtyRef.current) return;
        e.preventDefault();
        const discard = await ask("This workbook has unsaved changes. Close it without saving?", {
          title: "Unsaved changes",
          kind: "warning",
          okLabel: "Close",
          cancelLabel: "Cancel",
        });
        if (discard) await win.destroy();
      }),
    ];
    // the file this window was opened for, handed over before we were listening
    api.takePendingOpen().then((paths) => {
      if (paths[0]) void openRef.current(paths[0]);
    });
    return () => {
      unlisten.forEach((u) => u.then((f) => f()));
    };
  }, []);

  useEffect(() => {
    if (!info) return;
    const title = `${info.name}${info.dirty ? " — Edited" : ""}`;
    getCurrentWindow().setTitle(title).catch(() => {});
  }, [info?.name, info?.dirty]); // eslint-disable-line react-hooks/exhaustive-deps

  const onGridKeyDown = (e: React.KeyboardEvent) => {
    if (e.nativeEvent.isComposing) return;
    const cmd = e.metaKey || e.ctrlKey;
    const extend = e.shiftKey;
    const step = (dr: number, dc: number) => {
      e.preventDefault();
      let r = dr;
      let c = dc;
      if (cmd) {
        // jump to the edge of the data
        const dim = info?.sheets[sheet];
        r = dr < 0 ? -sel.fr + 1 : dr > 0 ? (dim?.rows ?? 1) - sel.fr : 0;
        c = dc < 0 ? -sel.fc + 1 : dc > 0 ? (dim?.cols ?? 1) - sel.fc : 0;
      }
      if (extend) {
        setSel({ ...sel, fr: clamp(sel.fr + r, 1, rows), fc: clamp(sel.fc + c, 1, cols) });
      } else {
        // from the edge of a merged cell, so one key press leaves it
        const m = cmd ? null : anchorOf(merges, sel.fr, sel.fc);
        const baseR = !m ? sel.fr : r > 0 ? m.row + m.height - 1 : m.row;
        const baseC = !m ? sel.fc : c > 0 ? m.col + m.width - 1 : m.col;
        const t = anchorOf(merges, clamp(baseR + r, 1, rows), clamp(baseC + c, 1, cols));
        setSel({ ar: t.row, ac: t.col, fr: t.row, fc: t.col });
      }
    };
    switch (e.key) {
      case "ArrowUp": return step(-1, 0);
      case "ArrowDown": return step(1, 0);
      case "ArrowLeft": return step(0, -1);
      case "ArrowRight": return step(0, 1);
      case "PageUp": return step(-20, 0);
      case "PageDown": return step(20, 0);
      case "Tab":
        e.preventDefault();
        return moveActive(0, e.shiftKey ? -1 : 1);
      case "Enter":
        e.preventDefault();
        return moveActive(e.shiftKey ? -1 : 1, 0);
      case "Home":
        e.preventDefault();
        return setSel(cmd ? A1 : { ar: sel.ar, ac: 1, fr: sel.ar, fc: 1 });
      case "F2":
        e.preventDefault();
        return void startEdit();
      case "Delete":
      case "Backspace":
        e.preventDefault();
        return void clearSelection();
      case "Escape":
        return;
    }
    if (e.key.length === 1 && !cmd && !e.altKey) {
      e.preventDefault();
      void startEdit(e.key);
    }
  };

  // -------------------------------------------------------------- name box
  const rangeLabel =
    rect.width * rect.height > 1
      ? `${cellName(rect.row, rect.col)}:${cellName(rect.row + rect.height - 1, rect.col + rect.width - 1)}`
      : cellName(sel.ar, sel.ac);

  const commitName = () => {
    if (nameDraft === null) return;
    const ref = parseCellName(nameDraft);
    setNameDraft(null);
    if (ref && ref.row <= rows && ref.col <= cols) {
      setSel({ ar: ref.row, ac: ref.col, fr: ref.row, fc: ref.col });
    } else if (nameDraft.trim() !== "") {
      setToast(`"${nameDraft}" is not a valid cell reference`);
    }
    focusGrid();
  };

  // ----------------------------------------------------------------- render
  const dim = info?.sheets[sheet];
  const subtitle = !info
    ? ""
    : info.dirty
      ? "Edited"
      : info.path
        ? "Saved"
        : blank
          ? "New workbook"
          : "Imported · not saved yet";
  const fmt = (n: number) =>
    n.toLocaleString("en-US", { maximumFractionDigits: 4 });

  return (
    <div className="app">
      <header className="titlebar" data-tauri-drag-region>
        <div className="titlebar-lights" data-tauri-drag-region />
        <div className="app-icon" data-tauri-drag-region><AppMark /></div>
        <div className="doc-title" data-tauri-drag-region>
          <div className="doc-name" data-tauri-drag-region>
            {info?.name ?? "SheetBro"}
          </div>
          <div className="doc-sub" data-tauri-drag-region>
            {subtitle}
            {info?.path ? ` · ${info.path.replace(/^\/Users\/[^/]+/, "~")}` : ""}
          </div>
        </div>
        <div className="titlebar-actions">
          <button className="btn" onClick={doOpen}>Open</button>
          <button className="btn primary" onClick={doSave}>Save</button>
        </div>
      </header>

      <div className="toolbar">
        <button className="tb" title="Undo (⌘Z)" disabled={!info?.can_undo} onClick={() => mutate(api.undo())}><UndoIcon /></button>
        <button className="tb" title="Redo (⇧⌘Z)" disabled={!info?.can_redo} onClick={() => mutate(api.redo())}><RedoIcon /></button>
        <span className="sep" />
        <button className={`tb ${style?.bold ? "on" : ""}`} title="Bold (⌘B)" onClick={() => toggleStyle("bold")}><b>B</b></button>
        <button className={`tb ${style?.italic ? "on" : ""}`} title="Italic (⌘I)" onClick={() => toggleStyle("italic")}><i>I</i></button>
        <button className={`tb ${style?.underline ? "on" : ""}`} title="Underline (⌘U)" onClick={() => toggleStyle("underline")}><u>U</u></button>
        <label className="tb color" title="Fill color">
          <FillColorIcon color="#fdf3c8" />
          <input type="color" defaultValue="#fdf3c8" onChange={(e) => applyStyle("fill.color", e.target.value.toUpperCase())} />
        </label>
        <label className="tb color" title="Text color">
          <TextColorIcon color="#1f5a43" />
          <input type="color" defaultValue="#1f5a43" onChange={(e) => applyStyle("font.color", e.target.value.toUpperCase())} />
        </label>
        <button className="tb" title="Clear fill" onClick={() => applyStyle("fill.color", "")}><NoFillIcon /></button>
        <span className="sep" />
        <button className="tb" title="Align left" onClick={() => applyStyle("alignment.horizontal", "left")}><AlignLeftIcon /></button>
        <button className="tb" title="Align center" onClick={() => applyStyle("alignment.horizontal", "center")}><AlignCenterIcon /></button>
        <button className="tb" title="Align right" onClick={() => applyStyle("alignment.horizontal", "right")}><AlignRightIcon /></button>
        <button
          className={`tb ${touchesMerge ? "on" : ""}`}
          title={touchesMerge ? "Unmerge cells (⌥⌘M)" : "Merge & center (⌥⌘M)"}
          onClick={toggleMerge}
        >
          <MergeIcon />
        </button>
        <span className="sep" />
        <button className="tb mono" title="Currency" onClick={() => applyStyle("num_fmt", "$#,##0.00")}>$</button>
        <button className="tb mono" title="Percent" onClick={() => applyStyle("num_fmt", "0%")}>%</button>
        <button className="tb mono" title="Fewer decimals" onClick={() => applyStyle("num_fmt", adjustDecimals(style?.num_fmt ?? "general", -1))}>.0</button>
        <button className="tb mono" title="More decimals" onClick={() => applyStyle("num_fmt", adjustDecimals(style?.num_fmt ?? "general", 1))}>.00</button>
        <span className="sep" />
        <button
          className={`tb ${fnMenu ? "on" : ""}`}
          title="Functions"
          onMouseDown={(e) => e.preventDefault()}
          onClick={(e) => {
            const b = e.currentTarget.getBoundingClientRect();
            setFnMenu(fnMenu ? null : { left: b.left, top: b.bottom + 4 });
          }}
        >
          Σ
        </button>
        <button
          className={`tb mcp ${mcpOpen ? "on" : ""}`}
          title={mcp?.running ? `AI tools (MCP) · listening on port ${mcp.port}` : "AI tools (MCP)"}
          onClick={() => setMcpOpen(true)}
        >
          <McpIcon />
          <span>MCP</span>
          {mcp?.running && <i className="mcp-dot" />}
        </button>
      </div>
      {fnMenu && <FunctionMenu anchor={fnMenu} onPick={pickFunction} onClose={() => setFnMenu(null)} />}
      {mcpOpen && mcp && (
        <McpPanel
          status={mcp}
          onChange={setMcp}
          onError={showError}
          onClose={() => {
            setMcpOpen(false);
            focusGrid();
          }}
        />
      )}

      <div className="formula-bar">
        <input
          className="name-box"
          value={nameDraft ?? rangeLabel}
          spellCheck={false}
          onFocus={(e) => e.target.select()}
          onChange={(e) => setNameDraft(e.target.value)}
          onBlur={() => setNameDraft(null)}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitName();
            else if (e.key === "Escape") {
              setNameDraft(null);
              focusGrid();
            }
          }}
        />
        <span className="fx">fx</span>
        <div className="formula-field">
          <input
            ref={formulaInput}
            id="formula-input"
            className={`formula-input ${refs.length ? "tinted" : ""}`}
            value={editing ? editing.value : content}
            spellCheck={false}
            onFocus={() => !editing && setEditing({ value: content })}
            onChange={(e) => changeEdit(e.target.value)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing) return;
              if (e.key === "Enter") {
                e.preventDefault();
                commitEdit(1, 0);
              } else if (e.key === "Escape") {
                e.preventDefault();
                cancelEdit();
              }
            }}
          />
          {editing && refs.length > 0 && <FormulaMirror value={editing.value} refs={refs} input={formulaInput} />}
        </div>
        {hintCtx && hintDoc && (() => {
          const { name, args } = parseSyntax(hintDoc.syntax);
          const now = Math.min(hintCtx.arg, args.length - 1);
          return (
            <div className="fn-hint">
              {name}(
              {args.map((a, i) => (
                <span key={i}>
                  {i > 0 && ", "}
                  <span className={i === now ? "arg-now" : ""}>{a}</span>
                </span>
              ))}
              )<span className="desc">{hintDoc.desc}</span>
            </div>
          );
        })()}
      </div>

      <main className="grid-wrap">
        <Grid
          sheet={sheet}
          layout={layout}
          rows={rows}
          cols={cols}
          sel={sel}
          setSel={setSel}
          rev={rev}
          editing={editing}
          setEditValue={changeEdit}
          startEdit={() => void startEdit()}
          commitEdit={commitEdit}
          cancelEdit={cancelEdit}
          onKeyDown={onGridKeyDown}
          onResizeColumns={resizeColumns}
          onAutofitColumns={autofitColumns}
          canPoint={canPoint}
          onPoint={onPoint}
          onPointKey={onPointKey}
          pointFocus={pointFocus}
          refs={refs}
          preview={preview}
        />
        {blank && !info?.dirty && !editing && (
          <div className="empty-hint">
            <div className="empty-title">Open a spreadsheet</div>
            <div>
              Press <kbd>⌘O</kbd> or drop a <b>.csv</b> / <b>.xlsx</b> file here
            </div>
          </div>
        )}
        {dropping && <div className="drop-overlay">Drop to open</div>}
        {toast && <div className="toast">{toast}</div>}
      </main>

      <footer className="statusbar">
        <div className="tabs">
          {info?.sheets.map((s, i) => (
            <button
              key={i}
              className={`tab ${i === sheet ? "active" : ""}`}
              onClick={() => {
                if (i !== sheet) resetView(i);
              }}
            >
              {s.name}
            </button>
          ))}
          <button
            className="tab add"
            title="New sheet"
            onClick={async () => {
              try {
                const i = await api.addSheet();
                resetView(i.sheets.length - 1);
                applyInfo(i);
              } catch (e) {
                showError(e);
              }
            }}
          >
            <PlusIcon />
          </button>
        </div>
        <div className="stats">
          {stats && stats.count > 0 && (
            <>
              <span>Count <b>{fmt(stats.count)}</b></span>
              {stats.numeric > 0 && (
                <>
                  <span>Sum <b>{fmt(stats.sum)}</b></span>
                  <span>Avg <b>{fmt(stats.sum / stats.numeric)}</b></span>
                </>
              )}
            </>
          )}
          {dim && <span className="muted">{fmt(dim.rows)} rows × {fmt(dim.cols)} cols</span>}
        </div>
      </footer>
    </div>
  );
}
