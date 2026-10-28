/** Serialises a grid of strings the way Excel puts it on the clipboard. */
export function toTsv(rows: string[][]): string {
  const esc = (v: string) =>
    /[\t\n\r"]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
  return rows.map((r) => r.map(esc).join("\t")).join("\n");
}

/** Parses clipboard text (tab separated, optional quoted fields). */
export function parseTsv(text: string): string[][] {
  const src = text.replace(/\r\n?/g, "\n").replace(/\n$/, "");
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let quoted = false;
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (quoted) {
      if (ch === '"') {
        if (src[i + 1] === '"') {
          field += '"';
          i++;
        } else quoted = false;
      } else field += ch;
    } else if (ch === '"' && field === "") quoted = true;
    else if (ch === "\t") {
      row.push(field);
      field = "";
    } else if (ch === "\n") {
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
    } else field += ch;
  }
  row.push(field);
  rows.push(row);
  return rows;
}

/** Adds/removes decimal places to an Excel-style number format. */
export function adjustDecimals(fmt: string, delta: number): string {
  if (fmt === "general") return delta > 0 ? "0.0" : fmt;
  const m = /^([^0]*[#0,]*0)(?:\.(0*))?(.*)$/.exec(fmt);
  if (!m) return fmt;
  const n = Math.max(0, (m[2]?.length ?? 0) + delta);
  return m[1] + (n > 0 ? "." + "0".repeat(n) : "") + m[3];
}
