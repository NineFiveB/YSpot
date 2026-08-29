// Virtualized results list (SPEC.md §5.6, §5.10): fixed 48 px rows, windowed
// rendering from index math only (no DOM measurement), memoized rows keyed by
// the stable `volumeIdx:frn` id, <mark> highlights from UTF-16 code-unit
// ranges sliced directly off `name` (§5.13).

import {
  memo,
  useEffect,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { rowKey, type ResultItem } from "../lib/ipc";

export const ROW_HEIGHT = 48;
/** 480 logical-px window minus the 64 px query bar — a constant, never a DOM read (§5.10). */
export const LIST_HEIGHT = 416;
const OVERSCAN = 5;

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
  item: ResultItem;
  index: number;
  top: number;
  isSelected: boolean;
  onActivate: (index: number) => void;
}

const Row = memo(function Row({
  item,
  index,
  top,
  isSelected,
  onActivate,
}: RowProps): ReactElement {
  return (
    <div
      id={`row-${rowKey(item.id)}`}
      role="option"
      aria-selected={isSelected}
      className={isSelected ? "row row-selected" : "row"}
      style={{ transform: `translateY(${top}px)` }}
      onClick={() => onActivate(index)}
    >
      <div className="row-name">{renderHighlighted(item.name, item.matchRanges)}</div>
      <div className="row-path">{item.path}</div>
    </div>
  );
});

interface ResultsListProps {
  items: ResultItem[];
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
      <Row
        key={rowKey(item.id)}
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
