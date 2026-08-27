// Folder-kind heuristics shared by every surface that must decide whether
// "Delete" means "move to Trash" or "permanently delete". Only inside a Trash
// view is deletion irreversible, so only there does the UI ask for confirmation.

/** True when `folder` is the mailbox's Trash (or a provider's equivalent). */
export function looksLikeTrash(folder: string): boolean {
  const leaf = (folder ?? '').split(/[/.]/).pop()?.trim().toLowerCase() ?? '';
  return leaf === 'trash' || leaf === 'deleted items' || leaf === 'deleted messages';
}
