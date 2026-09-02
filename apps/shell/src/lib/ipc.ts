// Typed wrappers over the Tauri IPC bridge — SPEC.md §4.6, M0 subset.
//
// The shell relays indexd `SearchResults` pipe frames as `search:results`
// events; FRNs travel as decimal strings because a u64 exceeds the JS
// safe-integer range (2^53).

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

export interface IndexStatePayload {
  connected?: boolean;
  [key: string]: unknown;
}

/** Stable row key (§5.6): `volumeIdx:frn`. */
export function rowKey(id: ResultId): string {
  return `${id.volumeIdx}:${id.frn}`;
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

export function executeAction(path: string): Promise<unknown> {
  return invoke("execute_action", { path });
}

export function getStatus(): Promise<unknown> {
  return invoke("get_status");
}

export function onSearchResults(
  cb: (payload: SearchResultsPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchResultsPayload>("search:results", (e) => cb(e.payload));
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
