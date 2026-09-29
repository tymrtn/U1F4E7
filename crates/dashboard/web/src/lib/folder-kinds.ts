// Folder-kind heuristics shared by every surface that must decide whether
// "Delete" means "move to Trash" or "permanently delete". Only inside a Trash
// view is deletion irreversible, so only there does the UI ask for confirmation.

/** True when `folder` is the mailbox's Trash (or a provider's equivalent). */
export function looksLikeTrash(folder: string): boolean {
  const leaf = (folder ?? '').split(/[/.]/).pop()?.trim().toLowerCase() ?? '';
  return leaf === 'trash' || leaf === 'deleted items' || leaf === 'deleted messages';
}

/** True when `folder` is the account's Junk/Spam mailbox under any of the
 *  provider spellings the backend resolves `\Junk` to (Gmail `[Gmail]/Spam`,
 *  Exchange/WorkMail `Junk Email`/`Junk E-mail`, generic `Junk`/`Spam`). */
export function looksLikeJunk(folder: string): boolean {
  const leaf = (folder ?? '').split(/[/.]/).pop()?.trim().toLowerCase() ?? '';
  return (
    leaf === 'junk' ||
    leaf === 'spam' ||
    leaf === 'junk email' ||
    leaf === 'junk e-mail' ||
    leaf === 'bulk mail'
  );
}
