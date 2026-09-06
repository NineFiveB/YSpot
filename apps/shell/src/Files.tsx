// File Search, as a view inside the launcher (SPEC.md §7.3's "File Search
// command", in the in-place view shape of §5.9 as amended).
//
// The root list caps inline file hits at five so it stays scannable; this is
// where the full list lives. It is the same index and the same query path —
// one generation of the shared counter, results on the same event — with two
// differences: the page is much larger, and the query is parsed for §7.3's
// filters first. `kind:image`, `ext:rs,toml` and `path:src` combine with AND;
// the words left over are the name query.
//
// What is not here, and why, is recorded in docs/M1.md: `content:` routes to
// the full-text index, which is M2; the preview pane is a body of work of
// its own; and `kind:folder` needs a directory filter the protocol does not
// carry yet.

import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import * as ipc from "./lib/ipc";

interface Props {
  onClose: () => void;
}

export default function Files({ onClose }: Props): ReactElement {
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<ipc.Row[]>([]);
  const [selected, setSelected] = useState(0);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  /** The generation this view last asked for; anything else is not ours. */
  const genRef = useRef(0);

  const run = useCallback((text: string) => {
    // A generation from the shared counter, so the pipe's stale-drop
    // watermark stays monotonic across the root list and this view. The root
    // list ignores it because it is not the one IT issued, and vice versa.
    const gen = ipc.nextGen();
    genRef.current = gen;
    setRows([]);
    setSelected(0);
    setNote(null);
    setError(null);
    if (text.trim() === "") return;
    void ipc.filesSearch(gen, text).catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // Results arrive in batches on the same events the root list uses. Only
  // this view's own generation is taken, and batches append in score order —
  // §5.11's append-only rule holds here too, since a moving row under the
  // cursor is as wrong in a long list as a short one.
  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    const track = (p: Promise<() => void>): void => {
      void p.then((u) => {
        if (disposed) u();
        else unlisteners.push(u);
      });
    };
    track(
      ipc.onSearchResults((payload) => {
        if (payload.gen !== genRef.current) return;
        setRows((prev) => {
          const seen = new Set(prev.map((r) => r.key));
          const fresh = payload.items.map(ipc.fileRow).filter((r) => !seen.has(r.key));
          return fresh.length === 0 ? prev : [...prev, ...fresh];
        });
      }),
    );
    track(
      ipc.onSearchFallback((payload) => {
        if (payload.gen !== genRef.current) return;
        setRows(payload.items.map(ipc.fallbackFileRow));
        // Windows Search answers by name only; the filters are for the
        // service's index. Said, rather than silently applied to nothing.
        setNote(
          payload.unavailable
            ? `Not searchable: ${payload.unavailable}`
            : `${payload.reason} — filters do not apply to Windows Search results.`,
        );
      }),
    );
    return () => {
      disposed = true;
      unlisteners.forEach((u) => u());
    };
  }, []);

  const clamped = Math.min(selected, Math.max(0, rows.length - 1));
  const current = rows[clamped];

  const act = useCallback(
    (action: string) => {
      if (!current) return;
      void ipc.executeAction(current, action).catch((e) => setError(String(e)));
    },
    [current],
  );

  // Window-level, capture phase, like the other views: a click on a row must
  // not turn the view into a keyboard dead end.
  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.isComposing) return;
      const claim = (): void => {
        e.preventDefault();
        e.stopPropagation();
      };
      switch (e.key) {
        case "ArrowDown":
          claim();
          setSelected((s) => Math.min(s + 1, Math.max(0, rows.length - 1)));
          break;
        case "ArrowUp":
          claim();
          setSelected((s) => Math.max(0, s - 1));
          break;
        case "Enter":
          claim();
          act(e.ctrlKey ? "reveal" : "open");
          break;
        case "Escape":
          claim();
          onClose();
          break;
        default:
          break;
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [rows.length, onClose, act]);

  return (
    <div className="settings">
      <div className="settings-header">
        <button className="settings-back" onClick={onClose} aria-label="Back to search">
          ←
        </button>
        <h1>File Search</h1>
        <span className="settings-esc">Enter opens · Ctrl+Enter reveals · Esc to go back</span>
      </div>

      <div className="query-bar">
        <input
          ref={inputRef}
          className="query-input"
          type="text"
          value={query}
          placeholder="Name, with kind:image  ext:rs,toml  path:src …"
          spellCheck={false}
          autoComplete="off"
          autoFocus
          role="combobox"
          aria-expanded={rows.length > 0}
          aria-controls="files-listbox"
          aria-activedescendant={current ? `files-${current.key}` : undefined}
          onChange={(e) => {
            setQuery(e.target.value);
            run(e.target.value);
          }}
        />
      </div>

      <div className="clip-list" id="files-listbox" role="listbox" aria-label="File search results">
        {rows.length === 0 ? (
          <p className="settings-note">
            {query.trim() === ""
              ? "Type a name. Narrow it with kind:document, kind:image, kind:audio, kind:video, kind:archive, ext:pdf,docx or path:projects — they combine."
              : "No files match."}
          </p>
        ) : (
          rows.map((row, i) => (
            <div
              key={row.key}
              id={`files-${row.key}`}
              role="option"
              aria-selected={i === clamped}
              className={i === clamped ? "clip-row clip-row-selected" : "clip-row"}
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => setSelected(i)}
              onDoubleClick={() => act("open")}
            >
              <div className="clip-text">{row.name}</div>
              <div className="clip-meta">{row.subtitle}</div>
            </div>
          ))
        )}
      </div>

      {note ? <p className="settings-note">{note}</p> : null}
      {error ? <div className="settings-error">{error}</div> : null}
    </div>
  );
}
