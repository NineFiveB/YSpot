// YSpot launcher frontend — M0 (SPEC.md §5.6, §5.7, §5.10 subset).
//
// One query input that always holds focus; per-keystroke generation counter;
// stale-generation results dropped; batches applied at most once per
// animation frame; latency HUD as the M0 measurement instrument.

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactElement,
} from "react";
import { LIST_HEIGHT, ROW_HEIGHT, ResultsList } from "./components/ResultsList";
import * as ipc from "./lib/ipc";
import {
  markApplied,
  markKeydown,
  statsSnapshot,
  type LatencySnapshot,
} from "./lib/latency";
import { noteHidden, noteShown, startThrottleProbe } from "./lib/throttle";

const PAGE_ROWS = Math.max(1, Math.floor(LIST_HEIGHT / ROW_HEIGHT));

export default function App(): ReactElement {
  const [query, setQuery] = useState("");
  const [items, setItems] = useState<ipc.ResultItem[]>([]);
  const [selected, setSelected] = useState(0);
  const [generation, setGeneration] = useState(0);
  const [connected, setConnected] = useState(false);
  const [lat, setLat] = useState<LatencySnapshot>(statsSnapshot());

  const inputRef = useRef<HTMLInputElement>(null);
  const genRef = useRef(0);
  const appliedGenRef = useRef(0);
  const bufferRef = useRef<ipc.SearchResultsPayload[]>([]);
  const rafRef = useRef<number | null>(null);
  const keydownTsRef = useRef<number | null>(null);
  // §10 M0 non-injecting self-measurement: gen → resolver, fulfilled when that
  // generation's results are applied in a rAF. Lets a driver time the real
  // keydown→results-applied path without any OS input injection.
  const measureResolversRef = useRef<Map<number, (ms: number | null) => void>>(
    new Map(),
  );

  // Apply buffered result batches at most once per animation frame (§5.10).
  const applyBuffered = useCallback(() => {
    rafRef.current = null;
    const cur = genRef.current;
    const fresh = bufferRef.current.filter((p) => p.gen === cur);
    bufferRef.current = [];
    if (fresh.length === 0) return;
    fresh.sort((a, b) => a.seq - b.seq);
    const isNewGen = appliedGenRef.current !== cur;
    setItems((prev) => {
      // Service batches are strictly rank-descending — append-only (§5.11).
      const base = isNewGen ? [] : prev.slice();
      for (const p of fresh) base.push(...p.items);
      return base;
    });
    if (isNewGen) {
      appliedGenRef.current = cur;
      // Selection resets only on generation change, never on append (§5.7).
      setSelected(0);
      setGeneration(cur);
    }
    const applied = markApplied(cur);
    if (applied !== null) setLat(statsSnapshot());
    // §10 M0 harness endpoint: this rAF committed `cur`'s results. The final
    // flag says whether the committed set includes the generation's final
    // batch — the harness pairs `applied gen=N final=1` with the results
    // marker of the same generation, so a partial-batch commit can never be
    // mistaken for the completed one if batching ever appears (M0 sends
    // exactly one final batch, so today this is always 1).
    const sawFinal = fresh.some((p) => p.isFinal);
    ipc.m0Mark(`applied gen=${cur} final=${sawFinal ? 1 : 0}`);
    // Fulfil a self-measurement waiter for this generation.
    const resolve = measureResolversRef.current.get(cur);
    if (resolve) {
      measureResolversRef.current.delete(cur);
      resolve(applied);
    }
  }, []);

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
        bufferRef.current.push(payload);
        scheduleApply();
      }),
    );
    track(ipc.onIndexState((p) => setConnected(p.connected === true)));
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

  // Capture-phase keydown: typing always goes to the query field (§5.7), and
  // the raw keydown timestamp feeds the latency HUD (§5.10 instrument).
  useEffect(() => {
    const handler = (e: KeyboardEvent): void => {
      keydownTsRef.current = performance.now();
      const el = inputRef.current;
      if (el && document.activeElement !== el && !e.isComposing) {
        el.focus();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, []);

  // §10 M0 non-injecting self-measurement. Drives one real search generation
  // and resolves with keydown→results-applied ms (or null on timeout/no
  // results). Same path a keystroke takes — markKeydown, the pipe round trip,
  // the rAF apply — but triggered in-process, so it injects NO OS input and
  // needs no elevated ETW session or window focus.
  const measureOne = useCallback(
    (text: string) =>
      new Promise<number | null>((resolve) => {
        const gen = ++genRef.current;
        markKeydown(gen, performance.now());
        const timer = window.setTimeout(() => {
          if (measureResolversRef.current.delete(gen)) resolve(null);
        }, 2000);
        measureResolversRef.current.set(gen, (ms) => {
          window.clearTimeout(timer);
          resolve(ms);
        });
        void ipc.search(gen, text).catch(() => {
          if (measureResolversRef.current.delete(gen)) {
            window.clearTimeout(timer);
            resolve(null);
          }
        });
      }),
    [],
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
  // each query prefix's keydown→results-applied and report. Each prefix is one
  // sample, bucketed by length — the shape `yspot-m0 type` produces, but with
  // no OS input injection, no ETW session, and no window focus needed.
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

  const resetResults = useCallback((gen: number) => {
    appliedGenRef.current = gen;
    bufferRef.current = [];
    setItems([]);
    setSelected(0);
    setGeneration(gen);
  }, []);

  function handleChange(e: ChangeEvent<HTMLInputElement>): void {
    const text = e.target.value;
    setQuery(text);
    const gen = ++genRef.current;
    if (text === "") {
      // Empty query: clear locally, but still dispatch — the higher gen
      // cancels the previous query's in-flight work service-side (§4.4),
      // and an empty text yields an empty final batch.
      resetResults(gen);
      void ipc.search(gen, "").catch(() => {});
      return;
    }
    markKeydown(gen, keydownTsRef.current ?? performance.now());
    keydownTsRef.current = null;
    void ipc.search(gen, text).catch((err) => {
      console.warn("search invoke failed", err);
    });
  }

  const maxIndex = Math.max(0, items.length - 1);
  const clampSel = (i: number): number => Math.max(0, Math.min(i, maxIndex));
  const selClamped = clampSel(selected);

  const activateIndex = useCallback(
    (index: number) => {
      const item = items[index];
      if (item) {
        void ipc.executeAction(item.path).catch((err) => {
          console.warn("executeAction failed", err);
        });
      }
    },
    [items],
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
        setSelected((s) => clampSel(s + 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelected((s) => clampSel(s - 1));
        break;
      case "PageDown":
        e.preventDefault();
        setSelected((s) => clampSel(s + PAGE_ROWS));
        break;
      case "PageUp":
        e.preventDefault();
        setSelected((s) => clampSel(s - PAGE_ROWS));
        break;
      case "Enter":
        e.preventDefault();
        activateIndex(selClamped);
        break;
      case "Escape":
        e.preventDefault();
        if (query !== "") {
          // Esc clears a non-empty query; only an empty query dismisses (§5.7).
          setQuery("");
          resetResults(++genRef.current);
        } else {
          void ipc.hideWindow().catch(() => undefined);
        }
        break;
      default:
        break;
    }
  }

  const selectedItem = items[selClamped];
  const hud = [
    lat.last !== null ? `${lat.last.toFixed(1)} ms` : "– ms",
    `p50 ${lat.p50 !== null ? lat.p50.toFixed(1) : "–"}`,
    `p95 ${lat.p95 !== null ? lat.p95.toFixed(1) : "–"}`,
    `${items.length} results`,
    connected ? "indexd: connected" : "indexd: offline",
  ].join(" · ");

  return (
    <div className="app">
      <div className="query-bar">
        <input
          ref={inputRef}
          className="query-input"
          type="text"
          value={query}
          placeholder="Search files…"
          spellCheck={false}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          autoFocus
          role="combobox"
          aria-expanded={items.length > 0}
          aria-controls="results-listbox"
          aria-activedescendant={
            selectedItem ? `row-${ipc.rowKey(selectedItem.id)}` : undefined
          }
          onChange={handleChange}
          onKeyDown={handleKeyDown}
        />
      </div>
      <ResultsList
        items={items}
        selected={selClamped}
        generation={generation}
        onActivate={onActivate}
      />
      <div className="hud" aria-hidden="true">
        {hud}
      </div>
    </div>
  );
}
