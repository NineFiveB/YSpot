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

/**
 * File results from the shell's own Windows Search provider (§3.1, §9.5) —
 * portable mode, or a scope the service does not index.
 */
/** One unranked Windows Search hit, scored by the shell (§5.11 rule 2). */
export interface FallbackItem {
  path: string;
  name: string;
  score: number;
}

export interface SearchFallbackPayload {
  gen: number;
  items: FallbackItem[];
  /** Why the shell answered instead of the service. */
  reason: string;
  /** Set when Windows Search itself could not answer (§3.1: never silent). */
  unavailable: string | null;
}

export function onSearchFallback(
  cb: (payload: SearchFallbackPayload) => void,
): Promise<UnlistenFn> {
  return listen<SearchFallbackPayload>("search:fallback", (e) => cb(e.payload));
}

/** One built-in YSpot command (§5.9, §7.6). */
export interface CommandItem {
  id: string;
  name: string;
  /** "YSpot", or the destination for a command that opens another app. */
  subtitle: string;
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
    subtitle: item.subtitle,
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

/**
 * A file the shell's own provider found. It has no volume/FRN identity — the
 * service is what mints those — so the path is the stable key.
 */
export function fallbackFileRow(item: FallbackItem): Row {
  return {
    kind: "file",
    key: `file:path:${item.path}`,
    id: item.path,
    name: item.name,
    subtitle: item.path,
    // Scored by the shell: a flat base below every catalog tier, since these
    // are unranked and should not outrank a match the shell scored, plus the
    // §7.1 frecency bonus for a path you have opened from here before.
    score: item.score,
    matchRanges: [],
    path: item.path,
  };
}

/**
 * Paths whose contents are never a destination, matched case-insensitively
 * against the start of a file's path.
 *
 * `WinSxS` is the component store. Every file in it is a hardlinked duplicate
 * of one that also exists somewhere usable — that is what the store is FOR —
 * so a hit there is always a worse copy of a hit available elsewhere. It is
 * also 145,427 files on this machine, 13% of a 1.09M-entry index, and it
 * carries only the Archive attribute, so §3.4's hidden/system penalty never
 * touches it.
 *
 * Measured, which is why this exists: the query `password` put three WinSxS
 * files above the iCloud Passwords app. A prefix hit at depth 3 scores
 * 0.9 x 0.943 = 0.849; the app matches at word-start for a flat 0.800.
 */
const DEPRIORITIZED_PREFIXES: readonly string[] = [
  "/windows/winsxs/",
  "/windows/servicing/",
  "/windows/assembly/",
  "/$recycle.bin/",
  "/system volume information/",
];

/**
 * How far a deprioritized path is pushed down.
 *
 * Enough that a prefix hit inside the component store loses to a word-start
 * hit outside it — 0.849 x 0.6 = 0.509 against the app's 0.800 — and small
 * enough that the file is still findable when nothing else matches, which
 * §7.3's "every file on every NTFS volume" promise requires. This demotes;
 * it never excludes.
 */
export const DEPRIORITIZED_FACTOR = 0.6;

/**
 * The demotion factor for a path, or 1 when it is an ordinary location.
 *
 * Separators are normalised and the drive letter is ignored, so the same rule
 * covers `C:` and a second volume with its own Windows directory. Matched
 * with a leading separator so a folder merely NAMED `winsxs` somewhere in the
 * user's own tree is untouched — only the real one under a Windows root.
 */
export function pathWeight(path: string): number {
  const p = path.toLowerCase().split("\\").join("/");
  const rooted = p.slice(p.indexOf("/"));
  return DEPRIORITIZED_PREFIXES.some((d) => rooted.startsWith(d)) ? DEPRIORITIZED_FACTOR : 1;
}

export function fileRow(item: ResultItem): Row {
  const id = rowKey(item.id);
  return {
    kind: "file",
    key: `file:${id}`,
    id,
    name: item.name,
    subtitle: item.path,
    score: item.score * pathWeight(item.path),
    matchRanges: item.matchRanges,
    path: item.path,
  };
}

/**
 * One generation counter for every consumer of the search events.
 *
 * The pipe client keeps a single stale-drop watermark and the service a single
 * cancellation watermark, both of which assume generations only go up. The
 * root list and the File Search view both issue queries, so they draw from
 * the same counter and each keeps only the results for the generation IT
 * issued last.
 */
let generation = 0;

export function nextGen(): number {
  return ++generation;
}

/** What `nextGen` will return next, for callers that must predict it. */
export function peekNextGen(): number {
  return generation + 1;
}

export function search(gen: number, text: string): Promise<unknown> {
  return invoke("search", { gen, text });
}

/** §7.3's File Search: the query is parsed for `kind:`/`ext:`/`path:` first. */
export function filesSearch(gen: number, text: string): Promise<unknown> {
  return invoke("files_search", { gen, text });
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
/**
 * The Windows icon for a row, or `null` when the row legitimately has none
 * and should keep its stroke glyph.
 *
 * Takes `(kind, id)` and never a parsing name: the shell derives the shell
 * item from its own catalogs, because any string reaching
 * `SHCreateItemFromParsingName` can activate a shell extension.
 */
export function rowIcon(kind: string, id: string, px: number): Promise<string | null> {
  return invoke<string | null>("row_icon", { kind, id, px });
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

/** §8.5 consent, opt-in and stored so onboarding sets it once. */
export interface Diagnostics {
  // snake_case, unlike the rest of the IPC surface. `Settings` is not just a
  // wire type — it IS the on-disk shape of settings.json — so the Rust side
  // carries no `rename_all`, and these keys have to match the file. Writing
  // `crashReports` here does not fail: `Settings` keeps unknown keys in a
  // flattened bag, so the wrong spelling was persisted as junk while the
  // real consent silently stayed false.
  crash_reports: boolean;
}

/** §7.4 clipboard settings. Same snake_case rule as `Diagnostics`. */
export interface ClipboardSettings {
  capture: boolean;
}

/**
 * Who registers the global chord (§5.1 as amended). "shell" is YSpot's own
 * `RegisterHotKey`; "ykeys" means the YKeys daemon owns the keyboard and
 * summons the launcher by posting a window message instead.
 */
export type HotkeySource = "shell" | "ykeys";

export interface Settings {
  hotkey: Hotkey;
  hotkey_source: HotkeySource;
  theme: "system" | "light" | "dark";
  diagnostics: Diagnostics;
  clipboard: ClipboardSettings;
  [key: string]: unknown;
}

export interface SettingsView {
  settings: Settings;
  autostart: boolean;
  hotkeyWarning: string | null;
  /**
   * Set while the bound chord is not actually registered (§5.1). Distinct
   * from `hotkeyWarning`, which is a caution about a chord that did bind.
   */
  hotkeyError: string | null;
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

// ---------------------------------------------------------------------------
// §7.4 clipboard history.

export interface ClipItem {
  id: number;
  kind: "text" | "files";
  /** The first part of the entry; the full content stays in the shell. */
  preview: string;
  /** The app it was copied from, when the shell could resolve one. */
  source: string;
  /** Unix seconds. */
  ts: number;
  score: number;
  matchRanges: [number, number][];
}

/** An empty query lists the most recent entries. */
export function clipboardList(query: string): Promise<ClipItem[]> {
  return invoke<ClipItem[]>("clipboard_list", { query });
}

/** §7.4 paste: dismiss, restore focus, write the clipboard, inject Ctrl+V. */
export function clipboardPaste(id: number): Promise<unknown> {
  return invoke("clipboard_paste", { id });
}

export function clipboardDelete(id: number): Promise<unknown> {
  return invoke("clipboard_delete", { id });
}

export function clipboardClear(): Promise<unknown> {
  return invoke("clipboard_clear");
}

export function clipboardEnabled(): Promise<boolean> {
  return invoke<boolean>("clipboard_enabled");
}

export function clipboardSetEnabled(enabled: boolean): Promise<unknown> {
  return invoke("clipboard_set_enabled", { enabled });
}

/** The shell asking the launcher to show its clipboard view (§7.4). */
export function onOpenClipboardView(cb: () => void): Promise<UnlistenFn> {
  return listen("view:clipboard", () => cb());
}

/** The shell asking the launcher to show File Search (§7.3). */
export function onOpenFilesView(cb: () => void): Promise<UnlistenFn> {
  return listen("view:files", () => cb());
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

/** What §5.9's first-run wizard needs to report truthfully. */
export interface OnboardingState {
  hotkey: Hotkey;
  hotkeyError: string | null;
  serviceConnected: boolean;
  autostart: boolean;
  crashReports: boolean;
}

export function onboardingState(): Promise<OnboardingState> {
  return invoke<OnboardingState>("onboarding_state");
}

/** Records that onboarding happened, so it is shown once (§5.9). */
export function finishOnboarding(): Promise<unknown> {
  return invoke("finish_onboarding");
}

/** The shell asking the launcher to show first-run onboarding (§5.9). */
export function onOpenOnboardingView(cb: () => void): Promise<UnlistenFn> {
  return listen("view:onboarding", () => cb());
}

/** §5.9's log export: open the folder the logs and dumps live in. */
export function openDiagnosticsFolder(): Promise<unknown> {
  return invoke("open_diagnostics_folder");
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
