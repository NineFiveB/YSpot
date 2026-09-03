// Typed wrappers over the Tauri IPC bridge — SPEC.md §4.6.
//
// The shell relays indexd `SearchResults` pipe frames as `search:results`
// events and answers the same generation's app matches as `search:apps`;
// FRNs travel as decimal strings because a u64 exceeds the JS safe-integer
// range (2^53).

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface ResultId {
  volumeIdx: number;
  /** NTFS file reference number as a decimal string (u64 > 2^53). */
  frn: string;
}

export interface ResultItem {
  id: ResultId;
  path: string;
  name: string;
  score: number;
  /** UTF-16 code-unit [start, end) ranges into `name` (§5.13). */
  matchRanges: [number, number][];
}

export interface SearchResultsPayload {
  gen: number;
  seq: number;
  isFinal: boolean;
  items: ResultItem[];
}

/** One app match from the shell's §7.1 `AppsFolder` catalog. */
export interface AppItem {
  /** AppUserModelID — the stable row id for apps (§5.6). */
  id: string;
  name: string;
  kind: "packaged" | "win32";
  score: number;
  matchRanges: [number, number][];
}

export interface SearchAppsPayload {
  gen: number;
  items: AppItem[];
}

export interface IndexStatePayload {
  connected?: boolean;
  [key: string]: unknown;
}

/**
 * One row of the merged result list. `kind` selects the provider; `key` is
 * the stable, provider-scoped row id of §5.6, and is what selection sticks
 * to across merges (§5.11).
 */
export type Row =
  | {
      kind: "app";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
      appKind: "packaged" | "win32";
    }
  | {
      kind: "file";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
      path: string;
    };

/** Stable row key for a file (§5.6): `volumeIdx:frn`. */
export function rowKey(id: ResultId): string {
  return `${id.volumeIdx}:${id.frn}`;
}

export function appRow(item: AppItem): Row {
  return {
    kind: "app",
    key: `app:${item.id}`,
    id: item.id,
    name: item.name,
    subtitle: item.kind === "packaged" ? "App" : "Application",
    score: item.score,
    matchRanges: item.matchRanges,
    appKind: item.kind,
  };
}

export function fileRow(item: ResultItem): Row {
  const id = rowKey(item.id);
  return {
    kind: "file",
    key: `file:${id}`,
    id,
    name: item.name,
    subtitle: item.path,
    score: item.score,
    matchRanges: item.matchRanges,
    path: item.path,
  };
}

export function search(gen: number, text: string): Promise<unknown> {
  return invoke("search", { gen, text });
}

export function hideWindow(): Promise<unknown> {
  return invoke("hide_window");
}

export function frontendReady(): Promise<unknown> {
  return invoke("frontend_ready");
}

/** §4.6 `executeAction` on the selected row. */
export function executeAction(
  row: Row,
  action: "open" | "runas" = "open",
): Promise<unknown> {
  return invoke("execute_action", {
    kind: row.kind,
    id: row.id,
    path: row.kind === "file" ? row.path : null,
    action,
  });
}

/** §7.1 icon for an app row, as a PNG data URI. Off the query path (§5.10). */
export function appIcon(id: string, px: number): Promise<string> {
  return invoke<string>("app_icon", { id, px });
}

export function getStatus(): Promise<unknown> {
  return invoke("get_status");
}

export function onSearchResults(
  cb: (payload: SearchResultsPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchResultsPayload>("search:results", (e) => cb(e.payload));
}

export function onSearchApps(
  cb: (payload: SearchAppsPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchAppsPayload>("search:apps", (e) => cb(e.payload));
}

export function onIndexState(
  cb: (payload: IndexStatePayload) => void,
): Promise<UnlistenFn> {
  return listen<IndexStatePayload>("index:state", (e) => cb(e.payload));
}

export function onWindowShown(cb: () => void): Promise<UnlistenFn> {
  return listen("window:shown", () => cb());
}

export function onWindowHidden(cb: () => void): Promise<UnlistenFn> {
  return listen("window:hidden", () => cb());
}

/**
 * Relay a §10 M0 measurement marker to the shell's ETW provider. Prefix
 * whitelisted on the Rust side; fire-and-forget by design — instrumentation
 * must never block or fail the UI path.
 */
export function m0Mark(text: string): void {
  void invoke("m0_mark", { text }).catch(() => undefined);
}

/**
 * Report the §10 M0 self-measurement result (keydown→results samples) to the
 * shell, which logs it. Non-injecting path; see App.tsx.
 */
export function m0Report(json: string): void {
  void invoke("m0_report", { json }).catch(() => undefined);
}

/**
 * The §10 M0 self-measurement request (`queries;iterations`), or "" when not
 * requested. Read once on startup; if set, the frontend drives its own real
 * search path and reports via {@link m0Report}.
 */
export function m0Spec(): Promise<string> {
  return invoke<string>("m0_spec").catch(() => "");
}
