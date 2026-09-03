// The Ctrl+K action panel (SPEC.md §5.7): a secondary palette over the
// selected row listing every action available on it with its shortcut,
// type-to-filter, and the same Up/Down/Enter/Esc rules as the main list.
//
// Focus moves into the panel's own filter input while it is open, so the
// §5.7 "typing always goes to the query field" rule hands over to the panel
// for as long as it lives; Esc closes it and returns focus to the query.

import { useEffect, useMemo, useRef, useState, type ReactElement } from "react";
import { filterActions, type Action } from "../lib/actions";

interface Props {
  actions: Action[];
  /** Title of the row the actions apply to, shown as the panel's heading. */
  subject: string;
  onRun: (actionId: string) => void;
  onClose: () => void;
}

export function ActionPanel({ actions, subject, onRun, onClose }: Props): ReactElement {
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const visible = useMemo(() => filterActions(actions, query), [actions, query]);
  const clamped = Math.min(selected, Math.max(0, visible.length - 1));

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  return (
    <div className="action-panel-scrim" onMouseDown={onClose}>
      <div
        className="action-panel"
        onMouseDown={(e) => e.stopPropagation()}
        role="dialog"
        aria-label={`Actions for ${subject}`}
      >
        <div className="action-panel-title">{subject}</div>
        <ul id="action-listbox" role="listbox" aria-label="Actions" className="action-list">
          {visible.map((a, i) => (
            <li
              key={a.id}
              id={`action-${a.id}`}
              role="option"
              aria-selected={i === clamped}
              className={
                (i === clamped ? "action-row action-row-selected" : "action-row") +
                (a.destructive ? " action-row-destructive" : "")
              }
              onClick={() => onRun(a.id)}
            >
              <span className="action-title">{a.title}</span>
              {a.shortcut ? <span className="action-shortcut">{a.shortcut}</span> : null}
            </li>
          ))}
          {visible.length === 0 ? <li className="action-row action-empty">No actions</li> : null}
        </ul>
        <input
          ref={inputRef}
          className="action-filter"
          type="text"
          value={query}
          placeholder="Filter actions…"
          spellCheck={false}
          autoComplete="off"
          role="combobox"
          aria-expanded={visible.length > 0}
          aria-controls="action-listbox"
          aria-activedescendant={visible[clamped] ? `action-${visible[clamped].id}` : undefined}
          onChange={(e) => {
            setQuery(e.target.value);
            setSelected(0);
          }}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing) return;
            switch (e.key) {
              case "ArrowDown":
                e.preventDefault();
                setSelected((s) => Math.min(s + 1, Math.max(0, visible.length - 1)));
                break;
              case "ArrowUp":
                e.preventDefault();
                setSelected((s) => Math.max(0, s - 1));
                break;
              case "Enter": {
                e.preventDefault();
                const a = visible[clamped];
                if (a) onRun(a.id);
                break;
              }
              case "Escape":
                e.preventDefault();
                onClose();
                break;
              default:
                break;
            }
          }}
        />
      </div>
    </div>
  );
}
