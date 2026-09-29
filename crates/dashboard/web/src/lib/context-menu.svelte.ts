// Which message row's right-click menu is open (#172). One menu at a time:
// right-clicking another row retargets it, and the mail layout closes it on
// navigation. The menu itself is MessageActionMenu in context mode, so every
// item still runs through the #170 action model.

export interface ContextMenuState {
  key: string;
  x: number;
  y: number;
  /** Where focus goes back to when the menu closes. */
  returnFocus: HTMLElement | null;
}

export class ContextMenuStore {
  current = $state<ContextMenuState | null>(null);

  openAt(state: ContextMenuState) {
    this.current = state;
  }

  isOpenFor(key: string): boolean {
    return this.current?.key === key;
  }

  close() {
    this.current = null;
  }
}

let singleton: ContextMenuStore | null = null;

export function getContextMenu(): ContextMenuStore {
  if (!singleton) singleton = new ContextMenuStore();
  return singleton;
}

/** Test-only reset. */
export function __resetContextMenu(): void {
  singleton = null;
}
