// YSpot launcher frontend (SPEC.md §5.6, §5.7, §5.10, §5.11, §7.1).
//
// One query input that always holds focus; per-keystroke generation counter;
// stale-generation results dropped; batches applied at most once per
// animation frame; apps (shell catalog, ~1 ms) and files (service, ~10 ms)
// merged by global score under the §5.11 selection contract; latency HUD as
// the M0 measurement instrument.

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactElement,
} from "react";
import Clipboard from "./Clipboard";
import Files from "./Files";
import Onboarding from "./Onboarding";
import Settings from "./Settings";
import { ActionPanel } from "./components/ActionPanel";
import { LIST_HEIGHT, ROW_HEIGHT, ResultsList } from "./components/ResultsList";
import { actionsFor, primaryAction, shortcutAction } from "./lib/actions";
import * as ipc from "./lib/ipc";
import { announceText } from "./lib/announce";
import { mergeRows, selectionIndex } from "./lib/merge";
import {
  markApplied,
  markKeydown,
  statsSnapshot,
  type LatencySnapshot,
} from "./lib/latency";
import { noteHidden, noteShown, startThrottleProbe } from "./lib/throttle";

const PAGE_ROWS = Math.max(1, Math.floor(LIST_HEIGHT / ROW_HEIGHT));

/** Logical window heights per view (§5.3 scales these by the monitor's DPI). */
const SEARCH_HEIGHT = 480;
const SETTINGS_HEIGHT = 620;
/** §7.3's inline cap on file rows in the root list (default 5). */
const INLINE_FILE_CAP = 5;
const CLIPBOARD_HEIGHT = 600;
const ONBOARDING_HEIGHT = 520;

/** Everything one generation has produced so far. */
interface GenState {
  gen: number;
  /** Everything the shell answered: calculator, apps, settings pages. */
  apps: ipc.Row[];
  files: ipc.Row[];
  /** Display order frozen at the moment the user moved the selection (§5.11). */
  frozen: ipc.Row[] | null;
  /** Stable key the selection is stuck to, or null while it sits on row 0. */
  selectedKey: string | null;
}

const emptyGen = (gen: number): GenState => ({
  gen,
  apps: [],
  files: [],
  frozen: null,
  selectedKey: null,
});

export default function App(): ReactElement {
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<ipc.Row[]>([]);
  const [selected, setSelected] = useState(0);
  const [generation, setGeneration] = useState(0);
  const [connected, setConnected] = useState(false);
  /** §9.5's unobtrusive hint, or §3.1's "not searchable" notice. */
  const [fallbackNote, setFallbackNote] = useState<string | null>(null);
  /** §5.12's polite live region, set only when a generation settles. */
  const [announcement, setAnnouncement] = useState("");
  const [panelOpen, setPanelOpen] = useState(false);
  // §5.7's navigation stack, one level deep for now: the results list, or a
  // view opened in place. Esc pops back.
  const [view, setView] = useState<
    "search" | "settings" | "clipboard" | "files" | "onboarding"
  >("search");
  const inSettings = view !== "search";
  const [lat, setLat] = useState<LatencySnapshot>(statsSnapshot());

  const inputRef = useRef<HTMLInputElement>(null);
  const panelOpenRef = useRef(false);
  const inSettingsRef = useRef(false);
  const genRef = useRef(0);
  const appliedGenRef = useRef(0);
  // Per-generation accumulation; mutated off the render path and committed in
  // the rAF below, so a burst of arrivals costs one render.
  const stateRef = useRef<GenState>(emptyGen(0));
  const filesBufferRef = useRef<ipc.SearchResultsPayload[]>([]);
  const fallbackBufferRef = useRef<ipc.SearchFallbackPayload[]>([]);
  const shellBufferRef = useRef<ipc.SearchShellPayload[]>([]);
  const rafRef = useRef<number | null>(null);
  const keydownTsRef = useRef<number | null>(null);
  /** The live query, for the announcement built inside the rAF callback. */
  const queryRef = useRef("");
  // §10 M0 non-injecting self-measurement: gen → resolver, fulfilled when that
  // generation's results are applied in a rAF. Lets a driver time the real
  // keydown→results-applied path without any OS input injection.
  const measureResolversRef = useRef<Map<number, (ms: number | null) => void>>(
    new Map(),
  );

  inSettingsRef.current = inSettings;

  /**
   * The list the last merge produced.
   *
   * Not the same thing as the `rows` state during the gap between a commit
   * and React rendering it: `applyBuffered` runs in a rAF callback, so its
   * `setRows` goes through the scheduler rather than flushing synchronously,
   * and Chromium dispatches a keydown queued in that window ahead of the
   * posted render. An arrow key landing there would otherwise freeze the
   * pre-merge order as if it were what the user could see — which appends
   * everything the merge had just interleaved to the bottom of the list, the
   * exact reorder-under-the-cursor §5.11 rule 3 exists to prevent.
   */
  const mergedRef = useRef<ipc.Row[]>([]);

  /** Recompute the display list and selection from the generation state. */
  const commit = useCallback((st: GenState) => {
    // §7.3: inline file hits are capped so the root list stays scannable;
    // the File Search command holds the full list. Top of the ranking, not
    // first to arrive — batches land in score order within themselves only.
    if (st.files.length > INLINE_FILE_CAP) {
      st.files.sort((a, b) => b.score - a.score);
      st.files.length = INLINE_FILE_CAP;
    }
    const merged = mergeRows(st);
    mergedRef.current = merged;
    setRows(merged);
    setSelected(selectionIndex(merged, st.selectedKey));
    return merged;
  }, []);

  // Apply buffered arrivals at most once per animation frame (§5.10).
  const applyBuffered = useCallback(() => {
    rafRef.current = null;
    const cur = genRef.current;
    const st = stateRef.current;
    const shell = shellBufferRef.current.filter((p) => p.gen === cur);
    const files = filesBufferRef.current.filter((p) => p.gen === cur);
    const fallback = fallbackBufferRef.current.filter((p) => p.gen === cur);
    shellBufferRef.current = [];
    filesBufferRef.current = [];
    fallbackBufferRef.current = [];
    if (shell.length === 0 && files.length === 0) return;
    for (const p of shell) {
      st.apps = [
        ...(p.calc ? [ipc.calcRow(p.calc)] : []),
        ...p.apps.map(ipc.appRow),
        ...p.windows.map(ipc.windowRow),
        ...p.commands.map(ipc.commandRow),
        ...p.settings.map(ipc.settingRow),
      ];
    }
    // Service batches are strictly rank-descending — append-only (§5.11 r1).
    files.sort((a, b) => a.seq - b.seq);
    for (const p of files) st.files.push(...p.items.map(ipc.fileRow));
    for (const p of fallback) st.files.push(...p.items.map(ipc.fallbackFileRow));
    const merged = commit(st);
    const isNewGen = appliedGenRef.current !== cur;
    if (isNewGen) {
      appliedGenRef.current = cur;
      setGeneration(cur);
    }
    // §2.5's endpoint is the SERVICE's results reaching the frontend, so the
    // measurement fires on the frame that commits them. Apps land a frame or
    // two earlier (shell catalog, ~1 ms) and deliberately do not consume the
    // pending keydown mark: `markApplied` is one-shot per generation, so an
    // apps-only frame taking it would leave the frame that actually applied
    // the service results with nothing to report.
    if (files.length > 0) {
      const applied = markApplied(cur);
      if (applied !== null) setLat(statsSnapshot());
      // §10 M0 harness endpoint. The final flag says whether the committed
      // set includes the generation's final batch — the harness pairs
      // `applied gen=N final=1` with the results marker of the same
      // generation, so a partial-batch commit can never be mistaken for the
      // completed one.
      const sawFinal = files.some((p) => p.isFinal);
      ipc.m0Mark(`applied gen=${cur} final=${sawFinal ? 1 : 0}`);
      // §5.12: announce the SETTLED set, not every batch. A reader saying
      // "1 result… 8 results… 23 results" for one keystroke is worse than
      // one that says nothing.
      if (sawFinal) {
        setAnnouncement(announceText(queryRef.current, merged));
      }
      const resolve = measureResolversRef.current.get(cur);
      if (resolve) {
        measureResolversRef.current.delete(cur);
        resolve(applied);
      }
    }
  }, [commit]);

  const scheduleApply = useCallback(() => {
    if (rafRef.current === null) {
      rafRef.current = requestAnimationFrame(applyBuffered);
    }
  }, [applyBuffered]);

  // IPC event listeners.
  useEffect(() => {
    let disposed = false;
    const unlisteners: (() => void)[] = [];
    const track = (p: Promise<() => void>): void => {
      void p.then((u) => {
        if (disposed) u();
        else unlisteners.push(u);
      });
    };
    track(
      ipc.onSearchResults((payload) => {
        // Drop stale generations before buffering (§4.4, §5.10).
        if (payload.gen !== genRef.current) return;
        filesBufferRef.current.push(payload);
        scheduleApply();
      }),
    );
    track(
      ipc.onSearchShell((payload) => {
        if (payload.gen !== genRef.current) return;
        shellBufferRef.current.push(payload);
        scheduleApply();
      }),
    );
    track(ipc.onIndexState((p) => setConnected(p.connected === true)));
    track(
      ipc.onSearchFallback((payload) => {
        if (payload.gen !== genRef.current) return;
        // Reuses the file buffer: these ARE the file results for this
        // generation, just from the shell's own provider (§9.5).
        filesBufferRef.current.push({
          gen: payload.gen,
          seq: 0,
          isFinal: true,
          items: [],
        });
        fallbackBufferRef.current.push(payload);
        setFallbackNote(
          payload.unavailable
            ? `File search unavailable: ${payload.unavailable}`
            : `${payload.reason} — install the full version for instant file search`,
        );
        scheduleApply();
      }),
    );
    track(ipc.onOpenSettingsView(() => setView("settings")));
    track(ipc.onOpenClipboardView(() => setView("clipboard")));
    track(ipc.onOpenFilesView(() => setView("files")));
    track(ipc.onOpenOnboardingView(() => setView("onboarding")));
    track(
      ipc.onViewReset(() => {
        setView("search");
        // The action panel is part of the surface the dismiss tore down. It
        // renders conditionally on `panelOpen && selectedItem`, so a flag
        // left set means it can remount over the next summon's results and
        // steal focus mid-typing — and because `panelOpenRef` gates §5.7's
        // refocus rule, a stale `true` leaves the launcher deaf to the
        // keyboard entirely.
        setPanelOpen(false);
      }),
    );
    track(
      ipc.onWindowShown(() => {
        noteShown();
        const el = inputRef.current;
        if (el) {
          el.focus();
          el.select();
        }
      }),
    );
    track(ipc.onWindowHidden(() => noteHidden()));
    return () => {
      disposed = true;
      for (const u of unlisteners) u();
      // A frame scheduled just before unmount must not fire afterwards.
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, [scheduleApply]);

  // The launcher grows to fit whatever view it is showing, and shrinks back
  // when that view closes (§5.3 recomputes the placement for the height).
  // The shell also learns which surface is up, so blur dismisses the results
  // list without closing a view whose dropdown just took focus.
  useEffect(() => {
    const height =
      view === "settings"
        ? SETTINGS_HEIGHT
        : view === "clipboard" || view === "files"
          ? CLIPBOARD_HEIGHT
          : view === "onboarding"
            ? ONBOARDING_HEIGHT
            : SEARCH_HEIGHT;
    void ipc.setLauncherHeight(height).catch(() => undefined);
    void ipc.setInView(view !== "search").catch(() => undefined);
  }, [view]);

  // The ref exists so the capture-phase key handler below can read the panel
  // state without re-subscribing on every toggle — not so it can hold a
  // second opinion about it. Deriving it here means anything that closes the
  // panel closes it for the refocus rule too, whether or not it remembered
  // to touch the ref.
  useEffect(() => {
    panelOpenRef.current = panelOpen;
  }, [panelOpen]);

  // Capture-phase keydown: typing always goes to the query field (§5.7), and
  // the raw keydown timestamp feeds the latency HUD (§5.10 instrument).
  useEffect(() => {
    const handler = (e: KeyboardEvent): void => {
      keydownTsRef.current = performance.now();
      // §5.7's refocus rule hands over while another surface owns the
      // keyboard: the action panel, or a view opened in place.
      if (panelOpenRef.current || inSettingsRef.current) return;
      const el = inputRef.current;
      if (el && document.activeElement !== el && !e.isComposing) {
        el.focus();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, []);

  const startGeneration = useCallback(
    (text: string): number => {
      // From the shared counter: the File Search view issues generations
      // too, and the pipe's watermark needs them all monotonic.
      const gen = ipc.nextGen();
      genRef.current = gen;
      stateRef.current = emptyGen(gen);
      shellBufferRef.current = [];
      filesBufferRef.current = [];
      fallbackBufferRef.current = [];
      setFallbackNote(null);
      // Selection resets to row 0 only here — on a generation change (§5.11).
      mergedRef.current = [];
      setRows([]);
      setSelected(0);
      appliedGenRef.current = gen;
      setGeneration(gen);
      void ipc.search(gen, text).catch((err) => {
        console.warn("search invoke failed", err);
      });
      return gen;
    },
    [],
  );

  // Leaving Settings puts focus back where §5.7 wants it: the query field.
  const closeView = useCallback(() => {
    setView("search");
    window.setTimeout(() => inputRef.current?.focus(), 0);
    // File Search draws generations from the same counter, and the pipe keeps
    // one watermark: a query typed in there supersedes whatever the root list
    // still had streaming, and those batches are dropped. Nothing would ever
    // re-issue them, so coming back showed however much of the list had
    // arrived before the view opened. Ask again.
    if (queryRef.current.trim() !== "") {
      startGeneration(queryRef.current);
    }
  }, [startGeneration]);

  // §10 M0 non-injecting self-measurement. Drives one real search generation
  // and resolves with keydown→results-applied ms (or null on timeout/no
  // results). Same path a keystroke takes — markKeydown, the pipe round trip,
  // the rAF apply — but triggered in-process, so it injects NO OS input and
  // needs no elevated ETW session or window focus.
  const measureOne = useCallback(
    (text: string) =>
      new Promise<number | null>((resolve) => {
        const gen = ipc.peekNextGen();
        markKeydown(gen, performance.now());
        const timer = window.setTimeout(() => {
          if (measureResolversRef.current.delete(gen)) resolve(null);
        }, 2000);
        measureResolversRef.current.set(gen, (ms) => {
          window.clearTimeout(timer);
          resolve(ms);
        });
        startGeneration(text);
      }),
    [startGeneration],
  );

  // First frame rendered → tell the shell the renderer is warm (§5.4).
  useEffect(() => {
    startThrottleProbe();
    const id = requestAnimationFrame(() => {
      void ipc.frontendReady().catch(() => undefined);
    });
    return () => cancelAnimationFrame(id);
  }, []);

  // §10 M0 non-injecting self-measurement, driven entirely in-page: if the
  // shell was started with a spec (`queries;iterations`), warm up, then time
  // each query prefix's keydown→results-applied and report.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const spec = await ipc.m0Spec();
      ipc.m0Report(JSON.stringify({ stage: "spec", spec }));
      if (!spec || cancelled) return;
      const [qsCsv, itStr] = spec.split(";");
      const iterations = Math.max(1, parseInt(itStr ?? "15", 10) || 15);
      const queries = qsCsv
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
      // Let the connection settle and warm the caches with one pass.
      await new Promise((r) => setTimeout(r, 1500));
      for (const q of queries) await measureOne(q);
      const rows: { q: string; plen: number; ms: number }[] = [];
      for (let it = 0; it < iterations && !cancelled; it++) {
        for (const q of queries) {
          for (let k = 1; k <= q.length; k++) {
            const ms = await measureOne(q.slice(0, k));
            if (ms !== null) rows.push({ q, plen: k, ms });
            await new Promise((r) => setTimeout(r, 15));
          }
        }
      }
      if (!cancelled) ipc.m0Report(JSON.stringify({ iterations, rows }));
    })();
    return () => {
      cancelled = true;
    };
  }, [measureOne]);

  function handleChange(e: ChangeEvent<HTMLInputElement>): void {
    const text = e.target.value;
    setQuery(text);
    queryRef.current = text;
    if (text !== "") {
      // The generation the keystroke is ABOUT to start, from the shared
      // counter: after the File Search view has issued queries, genRef + 1
      // is not it, and the latency sample for this keystroke would be lost.
      markKeydown(ipc.peekNextGen(), keydownTsRef.current ?? performance.now());
    }
    keydownTsRef.current = null;
    // An empty query still dispatches: the higher generation cancels the
    // previous query's in-flight work service-side (§4.4), and an empty text
    // yields an empty final batch and no app matches.
    startGeneration(text);
  }

  const maxIndex = Math.max(0, rows.length - 1);
  const clampSel = (i: number): number => Math.max(0, Math.min(i, maxIndex));

  /** Arrow/page keys: move the selection and stick it to that row (§5.11). */
  const moveSelection = useCallback((delta: number) => {
    setSelected((prev) => {
      // `mergedRef`, not the `rows` state: see the note on the ref. The two
      // agree except in the one frame that matters here.
      const list = mergedRef.current;
      const next = Math.max(0, Math.min(prev + delta, Math.max(0, list.length - 1)));
      const st = stateRef.current;
      const row = list[next];
      if (row) {
        st.selectedKey = row.key;
        // Freeze what is on screen: from here, late arrivals append below
        // rather than reorder around the user's cursor (§5.11 rule 3).
        st.frozen = list;
      }
      return next;
    });
  }, []);

  const activateIndex = useCallback(
    (index: number, action = "open") => {
      const row = rows[index];
      if (!row) return;
      void ipc.executeAction(row, action).catch((err) => {
        console.warn("executeAction failed", err);
      });
    },
    [rows],
  );

  const closePanel = useCallback(() => {
    panelOpenRef.current = false;
    setPanelOpen(false);
    inputRef.current?.focus();
  }, []);

  const runPanelAction = useCallback(
    (actionId: string) => {
      const index = Math.max(0, Math.min(selected, rows.length - 1));
      closePanel();
      activateIndex(index, actionId);
    },
    [activateIndex, closePanel, rows.length, selected],
  );
  // Stable identity for memoized rows: rows must not re-render just because
  // the items array (and thus this closure) was replaced (§5.6).
  const activateRef = useRef(activateIndex);
  activateRef.current = activateIndex;
  const onActivate = useCallback((index: number) => {
    activateRef.current(index);
  }, []);

  function handleKeyDown(e: ReactKeyboardEvent<HTMLInputElement>): void {
    // IME composition passes through untouched (§5.7, §5.13).
    if (e.nativeEvent.isComposing) return;
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        moveSelection(1);
        break;
      case "ArrowUp":
        e.preventDefault();
        moveSelection(-1);
        break;
      case "PageDown":
        e.preventDefault();
        moveSelection(PAGE_ROWS);
        break;
      case "PageUp":
        e.preventDefault();
        moveSelection(-PAGE_ROWS);
        break;
      case "Enter": {
        e.preventDefault();
        const row = rows[clampSel(selected)];
        if (!row) break;
        // A modifier chord runs its action directly (§5.7: every action is
        // keyboard-reachable, the common ones without the panel).
        const direct = shortcutAction(row, e);
        activateIndex(clampSel(selected), direct ?? primaryAction(row));
        break;
      }
      case "k":
      case "K":
        // §5.7: Ctrl+K opens the action panel over the selected row.
        if (e.ctrlKey && !e.altKey && rows[clampSel(selected)]) {
          e.preventDefault();
          panelOpenRef.current = true;
          setPanelOpen(true);
        }
        break;
      case "c":
      case "C":
      case "e":
      case "E": {
        const row = rows[clampSel(selected)];
        const direct = row ? shortcutAction(row, e) : null;
        if (direct) {
          e.preventDefault();
          activateIndex(clampSel(selected), direct);
        }
        break;
      }
      case "Escape":
        e.preventDefault();
        if (query !== "") {
          // Esc clears a non-empty query; only an empty query dismisses (§5.7).
          setQuery("");
          queryRef.current = "";
          startGeneration("");
        } else {
          void ipc.hideWindow().catch(() => undefined);
        }
        break;
      default:
        break;
    }
  }

  const selClamped = clampSel(selected);
  const selectedItem = rows[selClamped];
  const shellCount = rows.filter((r) => r.kind !== "file").length;
  const hud = [
    lat.last !== null ? `${lat.last.toFixed(1)} ms` : "– ms",
    `p50 ${lat.p50 !== null ? lat.p50.toFixed(1) : "–"}`,
    `p95 ${lat.p95 !== null ? lat.p95.toFixed(1) : "–"}`,
    `${rows.length} results (${shellCount} from the shell)`,
    connected ? "indexd: connected" : "indexd: offline",
  ].join(" · ");

  // §5.7's navigation stack: a view opened in place replaces the results
  // list, and Esc inside it pops back to the query.
  if (view !== "search") {
    return (
      <div className="app">
        {view === "settings" ? (
          <Settings onClose={closeView} />
        ) : view === "onboarding" ? (
          <Onboarding onClose={closeView} />
        ) : view === "files" ? (
          <Files onClose={closeView} />
        ) : (
          <Clipboard onClose={closeView} />
        )}
      </div>
    );
  }

  return (
    <div className="app">
      <div className="query-bar">
        <input
          ref={inputRef}
          className="query-input"
          type="text"
          value={query}
          aria-label="Search apps, files, settings and windows"
          placeholder="Search apps and files…"
          spellCheck={false}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          autoFocus
          role="combobox"
          aria-expanded={rows.length > 0}
          aria-controls="results-listbox"
          aria-activedescendant={selectedItem ? `row-${selectedItem.key}` : undefined}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
        />
      </div>
      <ResultsList
        items={rows}
        selected={selClamped}
        generation={generation}
        onActivate={onActivate}
      />
      {/* §5.12: result-count changes announced politely, off-screen. */}
      <div role="status" aria-live="polite" className="sr-only">
        {announcement}
      </div>
      {fallbackNote ? <div className="fallback-note">{fallbackNote}</div> : null}
      <div className="hud" aria-hidden="true">
        {hud}
      </div>
      {panelOpen && selectedItem ? (
        <ActionPanel
          actions={actionsFor(selectedItem)}
          subject={selectedItem.name}
          onRun={runPanelAction}
          onClose={closePanel}
        />
      ) : null}
    </div>
  );
}
