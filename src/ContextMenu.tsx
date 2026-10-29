import { ReactNode, useEffect, useLayoutEffect, useRef, useState } from "react";

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  /** shown at the right edge, like ⌘C */
  hint?: string;
  disabled?: boolean;
  onPick?: () => void;
  /** a flyout; the item itself does nothing when picked */
  submenu?: MenuItem[];
}
export type MenuEntry = MenuItem | "divider";

interface Props {
  /** viewport position of the right-click */
  at: { left: number; top: number };
  items: MenuEntry[];
  onClose: () => void;
}

const EDGE = 6;
/** room the flyout needs beside the menu */
const FLY_W = 200;

/** Indexes of the items that can be picked. */
const pickable = (list: MenuEntry[]) =>
  list.flatMap((e, i) => (e !== "divider" && !e.disabled ? [i] : []));

export function ContextMenu({ at, items, onClose }: Props) {
  const panel = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState(at);
  // highlighted item, and the highlighted item inside the open flyout
  const [hot, setHot] = useState<number | null>(null);
  const [subHot, setSubHot] = useState<number | null>(null);

  // keep the whole menu on screen
  useLayoutEffect(() => {
    const el = panel.current;
    if (!el) return;
    setPos({
      left: Math.max(EDGE, Math.min(at.left, window.innerWidth - el.offsetWidth - EDGE)),
      top: Math.max(EDGE, Math.min(at.top, window.innerHeight - el.offsetHeight - EDGE)),
    });
  }, [at.left, at.top]);

  const hotItem = hot !== null ? (items[hot] as MenuItem) : null;
  const flyOpen = !!hotItem?.submenu && !hotItem.disabled;
  const flip = pos.left + (panel.current?.offsetWidth ?? 0) + FLY_W > window.innerWidth;

  const pick = (it: MenuItem) => {
    if (it.disabled || it.submenu) return;
    onClose();
    it.onPick?.();
  };

  const onKey = (e: KeyboardEvent) => {
    // the menu is modal: nothing typed while it is open may reach the grid
    e.preventDefault();
    e.stopPropagation();
    const inFly = flyOpen && subHot !== null;
    const list = inFly ? hotItem!.submenu! : items;
    const cur = inFly ? subHot : hot;
    const ok = pickable(list);
    const step = (dir: 1 | -1) => {
      if (!ok.length) return;
      const from = cur === null ? -1 : ok.indexOf(cur);
      // from nothing, down goes to the first item and up to the last
      const next =
        from < 0 ? ok[dir === 1 ? 0 : ok.length - 1] : ok[(from + dir + ok.length) % ok.length];
      if (inFly) setSubHot(next);
      else {
        setHot(next);
        setSubHot(null);
      }
    };
    switch (e.key) {
      case "Escape":
        return onClose();
      case "ArrowDown":
        return step(1);
      case "ArrowUp":
        return step(-1);
      case "ArrowRight":
        if (flyOpen && !inFly) setSubHot(pickable(hotItem!.submenu!)[0] ?? null);
        return;
      case "ArrowLeft":
        return setSubHot(null);
      case "Enter":
      case " ":
        if (cur === null) return;
        if (!inFly && hotItem?.submenu) return setSubHot(pickable(hotItem.submenu)[0] ?? null);
        return pick(list[cur] as MenuItem);
    }
  };
  const keyRef = useRef(onKey);
  keyRef.current = onKey;
  useEffect(() => {
    const down = (e: KeyboardEvent) => keyRef.current(e);
    // capture: ahead of the grid's own key handling
    window.addEventListener("keydown", down, true);
    window.addEventListener("blur", onClose);
    window.addEventListener("resize", onClose);
    return () => {
      window.removeEventListener("keydown", down, true);
      window.removeEventListener("blur", onClose);
      window.removeEventListener("resize", onClose);
    };
  }, [onClose]);

  const row = (it: MenuItem, hovered: boolean, onHover: () => void, onClick: () => void) => (
    <button
      role="menuitem"
      className={`cm-item ${hovered ? "hot" : ""}`}
      disabled={it.disabled}
      aria-haspopup={it.submenu ? "menu" : undefined}
      onMouseEnter={onHover}
      onClick={onClick}
    >
      <span className="cm-icon">{it.icon}</span>
      <span className="cm-label">{it.label}</span>
      {it.hint && <span className="cm-hint">{it.hint}</span>}
      {it.submenu && <span className="cm-arrow">▸</span>}
    </button>
  );

  return (
    <>
      <div
        className="cm-backdrop"
        onMouseDown={(e) => {
          // leave focus on the grid
          e.preventDefault();
          onClose();
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          onClose();
        }}
      />
      <div
        ref={panel}
        role="menu"
        className="cm-panel"
        style={{ left: pos.left, top: pos.top }}
        // clicking an item must not pull focus off the grid
        onMouseDown={(e) => e.preventDefault()}
        onContextMenu={(e) => e.preventDefault()}
      >
        {items.map((it, i) =>
          it === "divider" ? (
            <div key={i} className="cm-divider" role="separator" />
          ) : (
            <div key={i} className="cm-slot">
              {row(
                it,
                hot === i,
                () => {
                  setHot(it.disabled ? null : i);
                  setSubHot(null);
                },
                () => pick(it),
              )}
              {it.submenu && hot === i && flyOpen && (
                <div role="menu" className={`cm-panel cm-fly ${flip ? "flip" : ""}`}>
                  {it.submenu.map((sub, j) => (
                    <div key={j} className="cm-slot">
                      {row(sub, subHot === j, () => setSubHot(sub.disabled ? null : j), () => pick(sub))}
                    </div>
                  ))}
                </div>
              )}
            </div>
          ),
        )}
      </div>
    </>
  );
}
