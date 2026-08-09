// Selection model for the v2 message list.
//
// Supports per-row checkbox selection, shift-click range selection, and
// keyboard 'x' toggle on the focused row. Exported as a Svelte 5 runes class
// (reactive properties) so layouts can instantiate a fresh store per box
// and pass it down to MessageRow / BulkToolbar.

export class SelectionStore {
  /** Currently selected UIDs. Svelte 5 rune — reactive. */
  selected = $state<Set<string>>(new Set());
  /** The UID of the last row clicked (anchor for shift-range). */
  private lastClickedKey: string | null = null;

  get count(): number {
    return this.selected.size;
  }

  get isEmpty(): boolean {
    return this.selected.size === 0;
  }

  /** Full replacement — called on box switch to clear selection. */
  clear() {
    this.selected = new Set();
    this.lastClickedKey = null;
  }

  isSelected(key: string): boolean {
    return this.selected.has(key);
  }

  toggle(key: string) {
    const next = new Set(this.selected);
    if (next.has(key)) {
      next.delete(key);
    } else {
      next.add(key);
    }
    this.selected = next;
    this.lastClickedKey = key;
  }

  /**
   * Range-select from the last clicked row to `key` (inclusive).
   * `orderedKeys` is the current full list of keys in display order.
   */
  rangeSelect(key: string, orderedKeys: string[]) {
    const anchor = this.lastClickedKey;
    if (!anchor) {
      this.toggle(key);
      return;
    }
    const ai = orderedKeys.indexOf(anchor);
    const bi = orderedKeys.indexOf(key);
    if (ai === -1 || bi === -1) {
      this.toggle(key);
      return;
    }
    const [from, to] = ai <= bi ? [ai, bi] : [bi, ai];
    const next = new Set(this.selected);
    for (let i = from; i <= to; i++) {
      next.add(orderedKeys[i]);
    }
    this.selected = next;
    this.lastClickedKey = key;
  }

  /** Called from the 'x' keyboard handler on the focused row. */
  keyToggle(key: string) {
    this.toggle(key);
  }

  selectAll(keys: string[]) {
    this.selected = new Set(keys);
  }

  deselectAll() {
    this.clear();
  }

  /** Remove exactly these keys, leaving every other selected key untouched.
   *  Used when only some items in a compound/partial operation actually
   *  succeeded — the rest must stay selected and retryable. */
  deselect(keys: Iterable<string>) {
    const next = new Set(this.selected);
    for (const key of keys) next.delete(key);
    this.selected = next;
  }
}
