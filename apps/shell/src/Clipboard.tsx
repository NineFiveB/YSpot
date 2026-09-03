// Clipboard history, as a view inside the launcher (SPEC.md §7.4, and the
// in-place view shape of §5.9 as amended).
//
// You search for it, filter it by typing, and Enter pastes into whatever you
// were doing — the launcher hides, focus goes back where it came from, and
// the paste keystroke follows. Esc leaves the view.
//
// The full contents never reach this component: it sees only the previews
// the shell decrypted into memory, and the shell reads the entry itself when
// something is actually pasted.

import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import * as ipc from "./lib/ipc";

/** A timestamp as something a person reads at a glance. */
function ago(ts: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (secs < 60) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** One line, so a multi-line entry does not blow up the row. */
function oneLine(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

interface Props {
  onClose: () => void;
}

export default function Clipboard({ onClose }: Props): ReactElement {
  const [query, setQuery] = useState("");
  const [items, setItems] = useState<ipc.ClipItem[]>([]);
  const [selected, setSelected] = useState(0);
  const [enabled, setEnabled] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const refresh = useCallback(
    (q: string) => {
      void ipc
        .clipboardList(q)
        .then((list) => {
          setItems(list);
          setSelected((s) => Math.min(s, Math.max(0, list.length - 1)));
        })
        .catch((e) => setError(String(e)));
    },
    [],
  );

  useEffect(() => {
    refresh("");
    void ipc.clipboardEnabled().then(setEnabled).catch(() => undefined);
    inputRef.current?.focus();
  }, [refresh]);

  const clamped = Math.min(selected, Math.max(0, items.length - 1));
  const current = items[clamped];

  const paste = useCallback(() => {
    if (!current) return;
    void ipc.clipboardPaste(current.id).catch((e) => setError(String(e)));
  }, [current]);

  const remove = useCallback(() => {
    if (!current) return;
    void ipc
      .clipboardDelete(current.id)
      .then(() => refresh(query))
      .catch((e) => setError(String(e)));
  }, [current, query, refresh]);

  function onKeyDown(e: React.KeyboardEvent<HTMLInputElement>): void {
    if (e.nativeEvent.isComposing) return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setSelected((s) => Math.min(s + 1, Math.max(0, items.length - 1)));
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelected((s) => Math.max(0, s - 1));
        break;
      case "Enter":
        e.preventDefault();
        paste();
        break;
      case "Delete":
        e.preventDefault();
        remove();
        break;
      case "Escape":
        e.preventDefault();
        onClose();
        break;
      default:
        break;
    }
  }

  return (
    <div className="settings">
      <div className="settings-header">
        <button className="settings-back" onClick={onClose} aria-label="Back to search">
          ←
        </button>
        <h1>Clipboard History</h1>
        <span className="settings-esc">Enter pastes · Esc to go back</span>
      </div>

      <div className="query-bar">
        <input
          ref={inputRef}
          className="query-input"
          type="text"
          value={query}
          placeholder="Filter clipboard history…"
          spellCheck={false}
          autoComplete="off"
          autoFocus
          role="combobox"
          aria-expanded={items.length > 0}
          aria-controls="clip-listbox"
          aria-activedescendant={current ? `clip-${current.id}` : undefined}
          onChange={(e) => {
            setQuery(e.target.value);
            setSelected(0);
            refresh(e.target.value);
          }}
          onKeyDown={onKeyDown}
        />
      </div>

      <div className="clip-list" id="clip-listbox" role="listbox" aria-label="Clipboard history">
        {items.length === 0 ? (
          <p className="settings-note">
            {enabled
              ? "Nothing in the history yet. Copy something and it will show up here."
              : "Capture is paused, so nothing new is being recorded."}
          </p>
        ) : (
          items.map((item, i) => (
            <div
              key={item.id}
              id={`clip-${item.id}`}
              role="option"
              aria-selected={i === clamped}
              className={i === clamped ? "clip-row clip-row-selected" : "clip-row"}
              onClick={() => setSelected(i)}
              onDoubleClick={() => paste()}
            >
              <div className="clip-text">{oneLine(item.preview)}</div>
              <div className="clip-meta">
                {item.kind === "files" ? "Files" : "Text"}
                {item.source ? ` · ${item.source}` : ""} · {ago(item.ts)}
              </div>
            </div>
          ))
        )}
      </div>

      <div className="clip-footer">
        <label className="clip-toggle">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => {
              const want = e.target.checked;
              void ipc
                .clipboardSetEnabled(want)
                .then(() => setEnabled(want))
                .catch((err) => setError(String(err)));
            }}
          />
          Record new items
        </label>
        <button
          className="clip-clear"
          onClick={() => {
            void ipc
              .clipboardClear()
              .then(() => refresh(query))
              .catch((e) => setError(String(e)));
          }}
        >
          Clear history
        </button>
      </div>

      {error ? <div className="settings-error">{error}</div> : null}
    </div>
  );
}
