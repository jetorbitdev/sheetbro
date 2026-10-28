import { useEffect, useMemo, useRef, useState } from "react";
import { FUNCTION_LIST } from "./functionList";
import { docFor } from "./formula";

export const QUICK_FUNCTIONS = ["SUM", "AVERAGE", "COUNT", "MAX", "MIN"];
const CATEGORIES = ["All", ...Object.keys(FUNCTION_LIST)];
const ALL_FUNCTIONS = Object.values(FUNCTION_LIST).flat().sort();
const MAX_RESULTS = 60;

interface Props {
  /** viewport position of the button the menu hangs from */
  anchor: { left: number; top: number };
  onPick: (name: string, quick: boolean) => void;
  onClose: () => void;
}

export function FunctionMenu({ anchor, onPick, onClose }: Props) {
  const [query, setQuery] = useState("");
  const [cat, setCat] = useState<{ name: string; top: number } | null>(null);
  const [hover, setHover] = useState<string | null>(null);
  const panel = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);

  useEffect(() => {
    search.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const results = useMemo(() => {
    const q = query.trim().toUpperCase();
    if (!q) return [];
    const starts = ALL_FUNCTIONS.filter((f) => f.startsWith(q));
    const contains = ALL_FUNCTIONS.filter((f) => !f.startsWith(q) && f.includes(q));
    return [...starts, ...contains].slice(0, MAX_RESULTS);
  }, [query]);

  const left = Math.min(anchor.left, window.innerWidth - 240);
  const flyLeft = left + 232 + 4 > window.innerWidth - 270 ? left - 270 : left + 232 + 4;
  const catItems = cat ? (cat.name === "All" ? ALL_FUNCTIONS : FUNCTION_LIST[cat.name]) : [];
  const hoverDoc = hover ? docFor(hover) : undefined;

  const item = (name: string, quick = false) => (
    <button
      key={name}
      className="fm-item"
      onMouseEnter={() => setHover(name)}
      onMouseLeave={() => setHover(null)}
      onClick={() => onPick(name, quick)}
    >
      {name}
    </button>
  );

  return (
    <>
      <div className="fm-backdrop" onMouseDown={onClose} />
      <div
        className="fm-panel"
        style={{ left, top: anchor.top }}
        ref={panel}
        onMouseDown={(e) => {
          // keep focus where it was (an active cell edit) unless the user is searching
          if ((e.target as HTMLElement).tagName !== "INPUT") e.preventDefault();
        }}
      >
        <input
          ref={search}
          className="fm-search"
          placeholder="Search functions…"
          value={query}
          spellCheck={false}
          onChange={(e) => {
            setQuery(e.target.value);
            setCat(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && results[0]) onPick(results[0], false);
          }}
        />
        <div className="fm-list">
          {query.trim() ? (
            results.length ? (
              results.map((f) => item(f))
            ) : (
              <div className="fm-empty">No function matches “{query}”</div>
            )
          ) : (
            <>
              {QUICK_FUNCTIONS.map((f) => item(f, true))}
              <div className="fm-divider" />
              {CATEGORIES.map((name) => (
                <button
                  key={name}
                  className={`fm-item fm-cat ${cat?.name === name ? "open" : ""}`}
                  onMouseEnter={(e) =>
                    setCat({ name, top: e.currentTarget.getBoundingClientRect().top })
                  }
                  onClick={(e) =>
                    setCat({ name, top: e.currentTarget.getBoundingClientRect().top })
                  }
                >
                  <span>{name}</span>
                  <span className="fm-arrow">▸</span>
                </button>
              ))}
            </>
          )}
        </div>
        {query.trim() && hoverDoc && (
          <div className="fm-hint">
            <code>{hoverDoc.syntax}</code>
            <span>{hoverDoc.desc}</span>
          </div>
        )}
      </div>

      {cat && !query.trim() && (
        <div
          className="fm-panel fm-fly"
          style={{
            left: flyLeft,
            top: Math.max(8, Math.min(cat.top - 6, window.innerHeight - 380)),
          }}
          onMouseDown={(e) => e.preventDefault()}
        >
          <div className="fm-fly-title">
            {cat.name} <span>{catItems.length}</span>
          </div>
          <div className="fm-list">{catItems.map((f) => item(f))}</div>
          <div className="fm-hint">
            {hoverDoc ? (
              <>
                <code>{hoverDoc.syntax}</code>
                <span>{hoverDoc.desc}</span>
              </>
            ) : (
              <span>{hover ? `${hover}(…)` : "Hover a function to see its syntax"}</span>
            )}
          </div>
        </div>
      )}
    </>
  );
}
