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

/** One Settings page or Control Panel item (§7.2). */
export interface SettingItem {
  id: string;
  name: string;
  /** "Settings" or "Control Panel" — the row's subtitle. */
  group: string;
  score: number;
  matchRanges: [number, number][];
}

/** One built-in YSpot command (§5.9, §7.6). */
export interface CommandItem {
  id: string;
  name: string;
  score: number;
  matchRanges: [number, number][];
}

/** One open window (§7.5). */
export interface WindowItem {
  /** The window handle as a string — stable for the window's lifetime. */
  id: string;
  title: string;
  process: string;
  score: number;
  matchRanges: [number, number][];
}

/** The calculator's answer (§7.7), when the query is an expression. */
export interface CalcItem {
  /** What the row shows, which may carry a unit or a base echo. */
  display: string;
  /** What Enter copies. */
  value: string;
}

/**
 * Everything the shell itself answers for one generation, in one event: all
 * of it is computed synchronously in the `search` command, so splitting it
 * would only add round trips and arrival orders to reason about.
 */
export interface SearchShellPayload {
  gen: number;
  apps: AppItem[];
  settings: SettingItem[];
  commands: CommandItem[];
  windows: WindowItem[];
  calc: CalcItem | null;
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
      kind: "calc";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
      value: string;
    }
  | {
      kind: "window";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
    }
  | {
      kind: "command";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
    }
  | {
      kind: "setting";
      key: string;
      id: string;
      name: string;
      subtitle: string;
      score: number;
      matchRanges: [number, number][];
    }
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

export function windowRow(item: WindowItem): Row {
  return {
    kind: "window",
    key: `window:${item.id}`,
    id: item.id,
    name: item.title,
    subtitle: item.process ? `Window — ${item.process}` : "Window",
    score: item.score,
    matchRanges: item.matchRanges,
  };
}

export function commandRow(item: CommandItem): Row {
  return {
    kind: "command",
    key: `command:${item.id}`,
    id: item.id,
    name: item.name,
    subtitle: "YSpot",
    score: item.score,
    matchRanges: item.matchRanges,
  };
}

export function settingRow(item: SettingItem): Row {
  return {
    kind: "setting",
    key: `setting:${item.id}`,
    id: item.id,
    name: item.name,
    subtitle: item.group,
    score: item.score,
    matchRanges: item.matchRanges,
  };
}

/**
 * §7.7 puts the calculator's answer first, so it carries a score above every
 * match tier rather than being special-cased in the merge.
 */
export function calcRow(item: CalcItem): Row {
  return {
    kind: "calc",
    key: "calc:result",
    id: "calc",
    name: item.display,
    subtitle: "Calculator — Enter copies",
    score: 2,
    matchRanges: [],
    value: item.value,
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

/** §4.6 `executeAction` on the selected row; `action` is a §7.1/§7.3 verb. */
export function executeAction(row: Row, action = "open"): Promise<unknown> {
  return invoke("execute_action", {
    kind: row.kind,
    id: row.id,
    // `path` carries the row's payload: a file's path, or the calculator's
    // value, which is what Enter copies (§7.7).
    path: row.kind === "file" ? row.path : row.kind === "calc" ? row.value : null,
    action,
  });
}

/** §7.1 icon for an app row, as a PNG data URI. Off the query path (§5.10). */
export function appIcon(id: string, px: number): Promise<string> {
  return invoke<string>("app_icon", { id, px });
}

// ---------------------------------------------------------------------------
// §5.9 Settings window.

/** The summon chord (§5.1); `code` is a `KeyboardEvent.code`. */
export interface Hotkey {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  win: boolean;
  code: string;
}

export interface Settings {
  hotkey: Hotkey;
  theme: "system" | "light" | "dark";
  [key: string]: unknown;
}

export interface SettingsView {
  settings: Settings;
  autostart: boolean;
  hotkeyWarning: string | null;
}

export function getSettings(): Promise<SettingsView> {
  return invoke<SettingsView>("get_settings");
}

/** Save through the shell (§5.9); a hotkey change is rebound first. */
export function saveSettings(next: Settings): Promise<SettingsView> {
  return invoke<SettingsView>("save_settings", { next });
}

export function openSettings(): Promise<unknown> {
  return invoke("open_settings");
}

/**
 * Grow or shrink the launcher in place (§5.3 placement, recomputed): the
 * window fits the view it is showing rather than a fixed results list.
 */
export function setLauncherHeight(logicalHeight: number): Promise<unknown> {
  return invoke("set_launcher_height", { logicalHeight });
}

/** The shell asking the launcher to show its Settings view (§5.9). */
export function onOpenSettingsView(cb: () => void): Promise<UnlistenFn> {
  return listen("view:settings", () => cb());
}

/** The window was dismissed: any view opened in place is gone (§5.2). */
export function onViewReset(cb: () => void): Promise<UnlistenFn> {
  return listen("view:reset", () => cb());
}

/**
 * Tell the shell which surface is showing. Blur dismisses the results list
 * (§5.2 step 3) but must not close a view with controls in it — a native
 * dropdown takes focus out of the webview.
 */
export function setInView(inView: boolean): Promise<unknown> {
  return invoke("set_in_view", { inView });
}

export function setAutostart(enabled: boolean): Promise<unknown> {
  return invoke("set_autostart", { enabled });
}

export function getStatus(): Promise<unknown> {
  return invoke("get_status");
}

export function onSearchResults(
  cb: (payload: SearchResultsPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchResultsPayload>("search:results", (e) => cb(e.payload));
}

export function onSearchShell(
  cb: (payload: SearchShellPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchShellPayload>("search:shell", (e) => cb(e.payload));
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
