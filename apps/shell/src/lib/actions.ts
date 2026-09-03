// The actions available on a result (SPEC.md §7.1, §7.3), and the shortcuts
// that reach them without opening the panel (§5.7).
//
// The first entry of the list is the row's PRIMARY action: what Enter runs.

import type { Row } from "./ipc";

export interface Action {
  /** The `action` value `executeAction` takes (§4.6). */
  id: string;
  title: string;
  /** Display form of the direct shortcut, when there is one (§5.7). */
  shortcut?: string;
  /** True for actions that change the filesystem — styled as destructive. */
  destructive?: boolean;
}

const FILE_ACTIONS: Action[] = [
  { id: "open", title: "Open", shortcut: "Enter" },
  { id: "reveal", title: "Reveal in Explorer", shortcut: "Ctrl+Shift+E" },
  { id: "copy_path", title: "Copy Path", shortcut: "Ctrl+Shift+C" },
  { id: "copy_file", title: "Copy File", shortcut: "Ctrl+C" },
  { id: "open_with", title: "Open With…" },
  { id: "delete", title: "Delete to Recycle Bin", destructive: true },
];

/** Every action for a row, primary first. */
export function actionsFor(row: Row): Action[] {
  if (row.kind === "file") return FILE_ACTIONS;
  const list: Action[] = [{ id: "open", title: "Open", shortcut: "Enter" }];
  // §7.1: "Run as administrator" is Win32 only — a packaged app cannot be
  // activated elevated, so the entry is not offered rather than offered and
  // failing.
  if (row.appKind === "win32") {
    list.push({ id: "runas", title: "Run as Administrator", shortcut: "Ctrl+Enter" });
  }
  return list;
}

/**
 * The action a modifier chord maps to for this row, or null when the chord
 * means nothing here. Keeps §5.7's "every action is keyboard-reachable"
 * true without going through the panel for the common ones.
 */
export function shortcutAction(
  row: Row,
  e: { key: string; ctrlKey: boolean; shiftKey: boolean; altKey: boolean },
): string | null {
  if (!e.ctrlKey || e.altKey) return null;
  const key = e.key.toLowerCase();
  if (row.kind === "app") {
    if (key === "enter" && !e.shiftKey && row.appKind === "win32") return "runas";
    return null;
  }
  if (key === "e" && e.shiftKey) return "reveal";
  if (key === "c") return e.shiftKey ? "copy_path" : "copy_file";
  return null;
}

/** Type-to-filter inside the panel: case-insensitive substring on the title. */
export function filterActions(actions: Action[], query: string): Action[] {
  const q = query.trim().toLowerCase();
  if (q === "") return actions;
  return actions.filter((a) => a.title.toLowerCase().includes(q));
}
