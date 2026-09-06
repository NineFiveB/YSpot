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
// Every §7.3 action is reachable here the way it is in the root list: the
// same modifier chords (Ctrl+Shift+E reveal, Ctrl+C / Ctrl+Shift+C copy) and
// the same Ctrl+K panel, over the same rows. §5.7 says every action is
// keyboard-reachable; a second surface with fewer verbs would break that
// quietly.
//
// What is not here, and why, is recorded in docs/M1.md: `content:` routes to
// the full-text index, which is M2; the preview pane is a body of work of
// its own; and `kind:folder` needs a directory filter the protocol does not
// carry yet.

import { useCallback, useEffect, useRef, useState, type ReactElement } from "react";
import { ActionPanel } from "./components/ActionPanel";
import { actionsFor, shortcutAction } from "./lib/actions";
import { announceText } from "./lib/announce";
import * as ipc from "./lib/ipc";

interface Props {
  onClose: () => void;
}

/** Roughly a viewport of rows, for PageUp/PageDown. */
const PAGE_STEP = 8;

export default function Files({ onClose }: Props): ReactElement {
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<ipc.Row[]>([]);
  const [selected, setSelected] = useState(0);
  /** True from the query being sent until its final batch (or fallback). */
  const [pending, setPending] = useState(false);
  const [announcement, setAnnouncement] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [panelOpen, setPanelOpen] = useState(false);
  /** The generation whose final batch has landed, for the announcement. */
  const [settled, setSettled] = useState(0);
  /** Windows Search itself could not answer (§3.1: never an empty list). */
  const [unavailable, setUnavailable] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  /** The generation this view last asked for; anything else is not ours. */
  const genRef = useRef(0);
  const queryRef = useRef("");
  /** Mirror of `rows`, readable outside React's render cycle. */
  const rowsRef = useRef<ipc.Row[]>([]);
  /**
   * Mirror of `pending`, for the disconnect listener below — it is subscribed
   * once and cannot see state. Written wherever `pending` is.
   */
  const pendingRef = useRef(false);

  const run = useCallback((text: string) => {
    // A generation from the shared counter, so the pipe's stale-drop
    // watermark stays monotonic across the root list and this view. The root
    // list ignores it because it is not the one IT issued, and vice versa.
    const gen = ipc.nextGen();
    genRef.current = gen;
    queryRef.current = text;
    rowsRef.current = [];
    setRows([]);
    setSelected(0);
    setNote(null);
    setError(null);
    setUnavailable(false);
    setAnnouncement("");
    // A new generation empties the list, and the panel is over a row of the
    // old one. Left open, it would unmount with its focus (no rows, no
    // `current`) while still suppressing this view's keys — a dead end.
    setPanelOpen(false);
    // Sent even when empty: the shell answers an empty query by cancelling
    // whatever this view had in flight, so clearing the box does not leave a
    // 100-row search running on the service for nothing.
    const inFlight = text.trim() !== "";
    pendingRef.current = inFlight;
    setPending(inFlight);
    void ipc.filesSearch(gen, text).catch((e) => {
      pendingRef.current = false;
      setPending(false);
      setError(String(e));
    });
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
          const next = fresh.length === 0 ? prev : [...prev, ...fresh];
          rowsRef.current = next;
          return next;
        });
        if (payload.isFinal) {
          pendingRef.current = false;
          setPending(false);
          // Announced from an effect keyed on the generation, not from inside
          // the updater above: React may run an updater later than the event
          // that queued it, past a reset for a newer generation.
          setSettled(payload.gen);
        }
      }),
    );
    track(
      ipc.onSearchFallback((payload) => {
        if (payload.gen !== genRef.current) return;
        const next = payload.items.map(ipc.fallbackFileRow);
        rowsRef.current = next;
        setRows(next);
        pendingRef.current = false;
        setPending(false);
        setUnavailable(payload.unavailable !== null);
        setSettled(payload.gen);
        // Windows Search answers by name only; the filters are for the
        // service's index. Said, rather than silently applied to nothing.
        setNote(
          payload.unavailable
            ? `Not searchable: ${payload.unavailable}`
            : `${payload.reason} — filters do not apply to Windows Search results.`,
        );
      }),
    );
    track(
      ipc.onIndexState((p) => {
        // A query in flight when the service goes away never gets its final
        // batch, and "Searching…" would sit there until the next keystroke.
        // Ask again: the shell now answers through Windows Search, and says
        // so in the note.
        //
        // In flight ONLY. A settled list is the user's — ranked, filtered,
        // with the cursor somewhere in it — and a service restart is not a
        // keystroke: §5.11 reserves a new generation for one. Replacing that
        // list with Windows Search's unranked, unfiltered answer would be
        // strictly worse, and the first version of this did exactly that.
        if (
          p.connected === false &&
          pendingRef.current &&
          genRef.current !== 0 &&
          queryRef.current.trim() !== ""
        ) {
          run(queryRef.current);
        }
      }),
    );
    return () => {
      disposed = true;
      unlisteners.forEach((u) => u());
    };
  }, [run]);

  // §5.12: the settled count, announced once per generation — and only if
  // that generation is still the one on screen.
  useEffect(() => {
    if (settled === 0 || settled !== genRef.current) return;
    setAnnouncement(announceText(queryRef.current, rowsRef.current));
  }, [settled]);

  const clamped = Math.min(selected, Math.max(0, rows.length - 1));
  const current = rows[clamped];

  // Keep the selected row in view. `nearest` scrolls only when it is off
  // screen, so arrowing inside the viewport does not jitter the list.
  useEffect(() => {
    if (!current) return;
    document
      .getElementById(`files-${current.key}`)
      ?.scrollIntoView({ block: "nearest" });
  }, [current]);

  // The panel takes focus on mount and leaves handing it back to its caller
  // — the root list does this in closePanel, and this view must too, or a
  // closed panel leaves focus on <body> and typing reaches nothing (§5.7).
  const closePanel = useCallback(() => {
    setPanelOpen(false);
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }, []);

  const act = useCallback(
    (action: string) => {
      if (!current) return;
      closePanel();
      void ipc.executeAction(current, action).catch((e) => setError(String(e)));
    },
    [current, closePanel],
  );

  // Window-level, capture phase, like the other views: a click on a row must
  // not turn the view into a keyboard dead end. While the action panel is
  // open it owns the keyboard.
  useEffect(() => {
    if (panelOpen) return;
    const last = Math.max(0, rows.length - 1);
    const onKey = (e: KeyboardEvent): void => {
      if (e.isComposing) return;
      const claim = (): void => {
        e.preventDefault();
        e.stopPropagation();
      };
      // §5.7's chords first, so Ctrl+C copies the file rather than the
      // query text, exactly as in the root list.
      if (current) {
        const chord = shortcutAction(current, e);
        if (chord) {
          claim();
          act(chord);
          return;
        }
      }
      switch (e.key) {
        case "ArrowDown":
          claim();
          setSelected((s) => Math.min(s + 1, last));
          break;
        case "ArrowUp":
          claim();
          setSelected((s) => Math.max(0, s - 1));
          break;
        case "PageDown":
          claim();
          setSelected((s) => Math.min(s + PAGE_STEP, last));
          break;
        case "PageUp":
          claim();
          setSelected((s) => Math.max(0, s - PAGE_STEP));
          break;
        case "End":
          if (e.ctrlKey) {
            claim();
            setSelected(last);
          }
          break;
        case "Home":
          if (e.ctrlKey) {
            claim();
            setSelected(0);
          }
          break;
        case "Enter":
          claim();
          act("open");
          break;
        case "k":
        case "K":
          if (e.ctrlKey && !e.altKey && current) {
            claim();
            setPanelOpen(true);
          }
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
  }, [rows.length, onClose, act, current, panelOpen]);

  const empty = query.trim() === "";

  return (
    <div className="settings">
      <div className="settings-header">
        <button className="settings-back" onClick={onClose} aria-label="Back to search">
          ←
        </button>
        <h1>File Search</h1>
        <span className="settings-esc">Enter opens · Ctrl+K actions · Esc to go back</span>
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
          aria-label="File search"
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
            {empty
              ? "Type a name. Narrow it with kind:document, kind:image, kind:audio, kind:video, kind:archive, ext:pdf,docx or path:projects — they combine."
              : pending
                ? "Searching…"
                : error || unavailable
                  ? ""
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
              <div className="clip-meta files-path" title={row.subtitle}>
                {row.subtitle}
              </div>
            </div>
          ))
        )}
      </div>

      {/* §5.12: the settled result count, announced politely, off-screen. */}
      <div role="status" aria-live="polite" className="sr-only">
        {announcement}
      </div>
      {note ? <p className="settings-note files-note">{note}</p> : null}
      {error ? <div className="settings-error">{error}</div> : null}

      {panelOpen && current ? (
        <ActionPanel
          actions={actionsFor(current)}
          subject={current.name}
          onRun={act}
          onClose={closePanel}
        />
      ) : null}
    </div>
  );
}
