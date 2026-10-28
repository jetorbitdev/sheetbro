import { useLayoutEffect, useRef, type ReactNode, type RefObject } from "react";
import type { FormulaRef } from "./formula";

const FOLLOW = ["scroll", "input", "keyup", "mouseup", "select", "focus"] as const;

/**
 * Paints a formula over its input (whose own text is made transparent) with
 * every reference in its color. Inputs can't color parts of their text.
 */
export function FormulaMirror(p: { value: string; refs: FormulaRef[]; input: RefObject<HTMLInputElement | null> }) {
  const box = useRef<HTMLDivElement>(null);

  // a long formula scrolls sideways inside the input; follow it
  useLayoutEffect(() => {
    const el = p.input.current;
    const mirror = box.current;
    if (!el || !mirror) return;
    const sync = () => {
      mirror.scrollLeft = el.scrollLeft;
    };
    sync();
    FOLLOW.forEach((e) => el.addEventListener(e, sync));
    return () => FOLLOW.forEach((e) => el.removeEventListener(e, sync));
  });

  const parts: ReactNode[] = [];
  let at = 0;
  for (const r of p.refs) {
    parts.push(p.value.slice(at, r.start));
    parts.push(
      <span key={r.start} style={{ color: r.color }}>
        {p.value.slice(r.start, r.end)}
      </span>,
    );
    at = r.end;
  }
  parts.push(p.value.slice(at));

  return (
    <div ref={box} className="formula-mirror" aria-hidden="true">
      <span>{parts}</span>
    </div>
  );
}
