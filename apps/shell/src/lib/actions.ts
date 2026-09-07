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

/**
 * §7.5's window verbs. Switching is the primary one; the layout presets and
 * the toggles live in the action panel, where a list is the right shape for
 * sixteen of them.
 */
const WINDOW_ACTIONS: Action[] = [
  { id: "open", title: "Switch to Window", shortcut: "Enter" },
  { id: "left_half", title: "Left Half" },
  { id: "right_half", title: "Right Half" },
  { id: "top_half", title: "Top Half" },
  { id: "bottom_half", title: "Bottom Half" },
  { id: "maximize", title: "Maximize" },
  { id: "center", title: "Center" },
  { id: "left_third", title: "Left Third" },
  { id: "center_third", title: "Center Third" },
  { id: "right_third", title: "Right Third" },
  { id: "left_two_thirds", title: "Left Two Thirds" },
  { id: "right_two_thirds", title: "Right Two Thirds" },
  { id: "top_left", title: "Top Left Quarter" },
  { id: "top_right", title: "Top Right Quarter" },
  { id: "bottom_left", title: "Bottom Left Quarter" },
  { id: "bottom_right", title: "Bottom Right Quarter" },
  // §7.5's move verb, next to the layouts because that is what it is: the
  // sixteenth way of putting the window somewhere.
  { id: "next_monitor", title: "Move to Next Display" },
  { id: "topmost", title: "Always on Top" },
  { id: "untopmost", title: "Not Always on Top" },
  { id: "minimize", title: "Minimize" },
  { id: "close", title: "Close Window", destructive: true },
];

/** Every action for a row, primary first. */
export function actionsFor(row: Row): Action[] {
  if (row.kind === "file") return FILE_ACTIONS;
  if (row.kind === "window") return WINDOW_ACTIONS;
  // §7.7: Enter copies the calculator's answer — there is nothing to open.
  if (row.kind === "calc") return [{ id: "copy", title: "Copy Result", shortcut: "Enter" }];
  if (row.kind === "setting" || row.kind === "command") {
    return [{ id: "open", title: "Open", shortcut: "Enter" }];
  }
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
/**
 * What plain Enter runs on a row: the first action, which every kind declares
 * with the `Enter` shortcut.
 *
 * Not the literal string "open". Every kind's primary action IS "open" except
 * the calculator's, which is "copy" — and hard-coding "open" made Enter and
 * Ctrl+K → Copy Result do two different things to the same row. Both copied
 * the answer, but only the panel's route kept the launcher open, because the
 * Rust side decides that from the action name (`stays_open` matches "copy",
 * not "open"). So pressing Enter on `12 mi in km` copied the answer and then
 * dismissed the launcher, while the row's own action table said otherwise.
 */
export function primaryAction(row: Row): string {
  return actionsFor(row)[0]?.id ?? "open";
}

export function shortcutAction(
  row: Row,
  e: { key: string; ctrlKey: boolean; shiftKey: boolean; altKey: boolean },
): string | null {
  if (!e.ctrlKey || e.altKey) return null;
  const key = e.key.toLowerCase();
  if (row.kind !== "file") {
    if (
      row.kind === "app" &&
      key === "enter" &&
      !e.shiftKey &&
      row.appKind === "win32"
    ) {
      return "runas";
    }
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
