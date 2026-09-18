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
import { rowIcon, type Row } from "../lib/ipc";

export const ROW_HEIGHT = 48;
/** 480 logical-px window minus the 64 px query bar — a constant, never a DOM read (§5.10). */
export const LIST_HEIGHT = 416;
const OVERSCAN = 5;
/** Logical icon size of §7.1; the shell scales it by the monitor's DPI. */
const ICON_LOGICAL_PX = 32;

/**
 * Process-wide icon cache: `kind:id` → data URI, or null once extraction has
 * failed or the row legitimately has no Windows icon (so a glyph row is asked
 * for once, not once per mount). Icons are immutable for the life of the
 * process, which is what makes a plain Map the right cache here (§5.10: off
 * the critical path, glyph until loaded).
 *
 * Keyed by kind AND id because ids are only unique within a kind — a command
 * and a settings page could otherwise collide and wear each other's icon.
 */
const iconCache = new Map<string, string | null>();
const iconWaiters = new Map<string, Set<(uri: string | null) => void>>();

function requestIcon(
  key: string,
  kind: string,
  id: string,
  px: number,
  cb: (uri: string | null) => void,
): () => void {
  const cached = iconCache.get(key);
  if (cached !== undefined) {
    cb(cached);
    return () => undefined;
  }
  let waiters = iconWaiters.get(key);
  if (!waiters) {
    waiters = new Set();
    iconWaiters.set(key, waiters);
    void rowIcon(kind, id, px)
      .then((uri) => finishIcon(key, uri))
      .catch(() => finishIcon(key, null));
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

/** Kinds whose rows can carry a real Windows icon; the rest keep a glyph. */
const ICONIC_KINDS = new Set<Row["kind"]>(["app", "command", "setting"]);

function useIcon(row: Row): string | null {
  const key = ICONIC_KINDS.has(row.kind) ? `${row.kind}:${row.id}` : null;
  const [uri, setUri] = useState<string | null>(() =>
    key ? (iconCache.get(key) ?? null) : null,
  );
  const { kind, id } = row;
  useEffect(() => {
    if (!key) return;
    const px = Math.round(ICON_LOGICAL_PX * (window.devicePixelRatio || 1));
    return requestIcon(key, kind, id, px, setUri);
  }, [key, kind, id]);
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

/**
 * One monochrome mark per row kind.
 *
 * Apps keep their real extracted icon (§7.1); every other kind gets a 16 px
 * stroke drawing in the same 32 px box. Two things this fixes at once: file
 * rows had `display: none` on their icon box, so their names started 42 px to
 * the left of every other row's, and seven rows all reading "Settings" were
 * visually identical with nothing but subtitle text to tell an app from a
 * folder from a command.
 *
 * Stroke-only and `currentColor`, so both themes and `forced-colors: active`
 * (§5.12) are correct with no palette of its own, and no state is conveyed by
 * colour. Static markup: no measurement and no layout read (§5.10).
 */
const KIND_GLYPH: Record<Row["kind"], string> = {
  // An app whose real icon could not be extracted still needs a mark, or the
  // row's name jumps 42 px left of every other row's.
  app: "M2.5 2.5h5v5h-5z M8.5 2.5h5v5h-5z M2.5 8.5h5v5h-5z M8.5 8.5h5v5h-5z",
  // A page with a folded corner.
  file: "M4.5 2.5h4l3 3v8h-7z M8.5 2.5v3h3",
  // Two sliders.
  setting: "M3 5h10 M3 11h10 M5 3.5h2v3h-2z M9 9.5h2v3h-2z",
  // A shell prompt: chevron and a line.
  command: "M3.5 4.5l3 3-3 3 M8.5 11.5h4",
  // A titled window.
  window: "M2.5 3.5h11v9h-11z M2.5 6.5h11",
  // An equals sign.
  calc: "M4 6.5h8 M4 9.5h8",
};

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
        {icon ? (
          <img src={icon} alt="" width={ICON_LOGICAL_PX} height={ICON_LOGICAL_PX} />
        ) : (
          <svg
            viewBox="0 0 16 16"
            width="16"
            height="16"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.25"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d={KIND_GLYPH[item.kind] ?? KIND_GLYPH.command} />
          </svg>
        )}
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
