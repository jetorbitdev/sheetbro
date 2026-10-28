import { parseCellName, type Rect } from "./api";
import { FUNCTION_DOCS } from "./functionDocs";

export interface FunctionContext {
  name: string;
  /** zero-based index of the argument being typed */
  arg: number;
}

/** The innermost function call that is still open at `caret`, if any. */
export function functionContext(formula: string, caret: number): FunctionContext | null {
  if (!formula.startsWith("=")) return null;
  const stack: FunctionContext[] = [];
  let inString = false;
  for (let i = 1; i < caret; i++) {
    const ch = formula[i];
    if (ch === '"') inString = !inString;
    if (inString) continue;
    if (ch === "(") {
      const name = /([A-Za-z][A-Za-z0-9._]*)$/.exec(formula.slice(0, i))?.[1] ?? "";
      stack.push({ name: name.toUpperCase(), arg: 0 });
    } else if (ch === ")") stack.pop();
    else if ((ch === "," || ch === ";") && stack.length) stack[stack.length - 1].arg++;
  }
  const top = stack[stack.length - 1];
  return top && top.name ? top : null;
}

/** Splits "SUM(a, [b], …)" into its name and argument labels. */
export function parseSyntax(syntax: string): { name: string; args: string[] } {
  const open = syntax.indexOf("(");
  const inner = syntax.slice(open + 1, syntax.lastIndexOf(")"));
  return {
    name: syntax.slice(0, open),
    args: inner.trim() === "" ? [] : inner.split(/,\s*/),
  };
}

export const docFor = (name: string) => FUNCTION_DOCS[name.toUpperCase()];

/**
 * Locales that write `=SUM(A1;A2)` (Indonesian, most European ones) work too:
 * semicolons outside string literals become commas.
 */
export function normalizeFormula(input: string): string {
  if (!input.startsWith("=")) return input;
  let out = "";
  let inString = false;
  for (const ch of input) {
    if (ch === '"') inString = !inString;
    out += ch === ";" && !inString ? "," : ch;
  }
  return out;
}

/** True when a click/drag should insert a cell reference at `caret`. */
export function expectsReference(value: string, caret: number): boolean {
  if (!value.startsWith("=")) return false;
  const before = value.slice(0, caret);
  if ((before.match(/"/g)?.length ?? 0) % 2 === 1) return false; // inside a string
  return /[=(,;+\-*/^&<>]\s*$/.test(before);
}

/** A cell or range reference inside a formula, with the color it is shown in */
export interface FormulaRef {
  start: number;
  end: number;
  rect: Rect;
  color: string;
}

export const REF_COLORS = ["#e8710a", "#1a73e8", "#9334e6", "#188038", "#d93025", "#0097a7", "#c2185b", "#8d6e63"];

// strings, other-sheet references, numbers and words; references are picked out of the words
const TOKEN =
  /"(?:[^"]|"")*"?|(?:'[^']*'|[A-Za-z_][\w.]*)!\$?[A-Za-z]+\$?\d+(?::\$?[A-Za-z]+\$?\d+)?|\d+(?:\.\d*)?(?:[eE][+-]?\d+)?|\$?[A-Za-z_][\w.]*\$?\d*(?::\$?[A-Za-z]+\$?\d+)?/g;
const REF = /^(\$?[A-Za-z]{1,3}\$?\d+)(?::(\$?[A-Za-z]{1,3}\$?\d+))?$/;

/**
 * References to cells of this sheet, in order. The same reference always gets
 * the same color, so the text and the box on the grid match.
 */
export function formulaRefs(formula: string): FormulaRef[] {
  if (!formula.startsWith("=")) return [];
  const refs: FormulaRef[] = [];
  const colors = new Map<string, string>();
  for (const m of formula.matchAll(TOKEN)) {
    const parts = REF.exec(m[0]);
    const end = m.index + m[0].length;
    if (!parts || formula[end] === "(") continue; // LOG10( is a function
    const a = parseCellName(parts[1]);
    const b = parseCellName(parts[2] ?? parts[1]);
    if (!a || !b || a.row < 1 || b.row < 1) continue;
    const key = m[0].replace(/\$/g, "").toUpperCase();
    if (!colors.has(key)) colors.set(key, REF_COLORS[colors.size % REF_COLORS.length]);
    refs.push({
      start: m.index,
      end,
      rect: {
        row: Math.min(a.row, b.row),
        col: Math.min(a.col, b.col),
        height: Math.abs(a.row - b.row) + 1,
        width: Math.abs(a.col - b.col) + 1,
      },
      color: colors.get(key)!,
    });
  }
  return refs;
}
