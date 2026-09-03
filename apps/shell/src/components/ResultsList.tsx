// Virtualized results list (SPEC.md §5.6, §5.10): fixed 48 px rows, windowed
// rendering from index math only (no DOM measurement), memoized rows keyed by
// the stable provider-scoped id (§5.6), <mark> highlights from UTF-16
// code-unit ranges sliced directly off `name` (§5.13).

import {
  memo,
  useEffect,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { appIcon, type Row } from "../lib/ipc";

export const ROW_HEIGHT = 48;
/** 480 logical-px window minus the 64 px query bar — a constant, never a DOM read (§5.10). */
export const LIST_HEIGHT = 416;
const OVERSCAN = 5;
/** Logical icon size of §7.1; the shell scales it by the monitor's DPI. */
const ICON_LOGICAL_PX = 32;

/**
 * Process-wide icon cache: app id → data URI, or null once extraction has
 * failed (so a broken icon is asked for once, not once per mount). Icons are
 * immutable for the life of the process, which is what makes a plain Map the
 * right cache here (§5.10: off the critical path, placeholder until loaded).
 */
const iconCache = new Map<string, string | null>();
const iconWaiters = new Map<string, Set<(uri: string | null) => void>>();

function requestIcon(id: string, px: number, cb: (uri: string | null) => void): () => void {
  const cached = iconCache.get(id);
  if (cached !== undefined) {
    cb(cached);
    return () => undefined;
  }
  let waiters = iconWaiters.get(id);
  if (!waiters) {
    waiters = new Set();
    iconWaiters.set(id, waiters);
    void appIcon(id, px)
      .then((uri) => finishIcon(id, uri))
      .catch(() => finishIcon(id, null));
  }
  waiters.add(cb);
  return () => {
    waiters?.delete(cb);
  };
}

function finishIcon(id: string, uri: string | null): void {
  iconCache.set(id, uri);
  const waiters = iconWaiters.get(id);
  iconWaiters.delete(id);
  if (waiters) for (const w of waiters) w(uri);
}

function useIcon(row: Row): string | null {
  const id = row.kind === "app" ? row.id : null;
  const [uri, setUri] = useState<string | null>(() =>
    id ? (iconCache.get(id) ?? null) : null,
  );
  useEffect(() => {
    if (!id) return;
    const px = Math.round(ICON_LOGICAL_PX * (window.devicePixelRatio || 1));
    return requestIcon(id, px, setUri);
  }, [id]);
  return uri;
}

function renderHighlighted(
  name: string,
  ranges: [number, number][] | undefined,
): ReactNode {
  if (!ranges || ranges.length === 0) return name;
  const out: ReactNode[] = [];
  let pos = 0;
  for (const range of ranges) {
    const start = range[0];
    const end = range[1];
    // Skip malformed or overlapping ranges rather than corrupting the row.
    if (start < pos || end <= start || start >= name.length) continue;
    if (start > pos) out.push(name.slice(pos, start));
    out.push(<mark key={start}>{name.slice(start, Math.min(end, name.length))}</mark>);
    pos = Math.min(end, name.length);
  }
  if (pos < name.length) out.push(name.slice(pos));
  return out;
}

interface RowProps {
  item: Row;
  index: number;
  top: number;
  isSelected: boolean;
  onActivate: (index: number) => void;
}

const ResultRow = memo(function ResultRow({
  item,
  index,
  top,
  isSelected,
  onActivate,
}: RowProps): ReactElement {
  const icon = useIcon(item);
  return (
    <div
      id={`row-${item.key}`}
      role="option"
      aria-selected={isSelected}
      className={isSelected ? "row row-selected" : "row"}
      style={{ transform: `translateY(${top}px)` }}
      onClick={() => onActivate(index)}
    >
      <div className={`row-icon row-icon-${item.kind}`} aria-hidden="true">
        {icon ? <img src={icon} alt="" width={ICON_LOGICAL_PX} height={ICON_LOGICAL_PX} /> : null}
      </div>
      <div className="row-text">
        <div className="row-name">{renderHighlighted(item.name, item.matchRanges)}</div>
        <div className="row-path">{item.subtitle}</div>
      </div>
    </div>
  );
});

interface ResultsListProps {
  items: Row[];
  selected: number;
  /** Bumps when a new generation's results are first applied — resets scroll. */
  generation: number;
  onActivate: (index: number) => void;
}

export function ResultsList({
  items,
  selected,
  generation,
  onActivate,
}: ResultsListProps): ReactElement {
  const containerRef = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const scrollTopRef = useRef(0);

  // New generation: jump back to the top.
  useEffect(() => {
    scrollTopRef.current = 0;
    setScrollTop(0);
    if (containerRef.current) containerRef.current.scrollTop = 0;
  }, [generation]);

  // Keep the selected row inside the viewport — pure index math, write-only
  // scroll mutation (§5.10: never read layout after a same-frame write).
  useEffect(() => {
    const selTop = selected * ROW_HEIGHT;
    const selBottom = selTop + ROW_HEIGHT;
    const st = scrollTopRef.current;
    let next: number | null = null;
    if (selTop < st) next = selTop;
    else if (selBottom > st + LIST_HEIGHT) next = selBottom - LIST_HEIGHT;
    if (next !== null) {
      scrollTopRef.current = next;
      setScrollTop(next);
      if (containerRef.current) containerRef.current.scrollTop = next;
    }
  }, [selected]);

  const total = items.length;
  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const last = Math.min(
    total,
    Math.ceil((scrollTop + LIST_HEIGHT) / ROW_HEIGHT) + OVERSCAN,
  );
  const rows: ReactElement[] = [];
  for (let i = first; i < last; i++) {
    const item = items[i];
    rows.push(
      <ResultRow
        key={item.key}
        item={item}
        index={i}
        top={i * ROW_HEIGHT}
        isSelected={i === selected}
        onActivate={onActivate}
      />,
    );
  }

  return (
    <div
      ref={containerRef}
      id="results-listbox"
      role="listbox"
      aria-label="Search results"
      className="results"
      style={{ height: LIST_HEIGHT }}
      onScroll={(e) => {
        scrollTopRef.current = e.currentTarget.scrollTop;
        setScrollTop(e.currentTarget.scrollTop);
      }}
    >
      <div className="results-inner" style={{ height: total * ROW_HEIGHT }}>
        {rows}
      </div>
    </div>
  );
}
