// IMAP flag spelling. The server reports flags in the index spelling
// (`"Seen"`, `"Flagged"` — async-imap's Debug form) on list, search, and read
// responses, while requests and older fixtures use the wire form (`\Seen`).
// Every surface asks through here so a row, the reader, and the toolbar can
// never disagree about the same message because they matched different
// spellings.

/** True when `flags` contains the system flag `name` in either spelling. */
export function hasFlag(flags: readonly string[] | null | undefined, name: string): boolean {
  const want = name.replace(/^\\/, '').toLowerCase();
  return (flags ?? []).some((f) => f.replace(/^\\/, '').toLowerCase() === want);
}
