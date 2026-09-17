<script lang="ts">
  import { installBodyFrameScrollBridge } from './body-frame-scroll';
  import { isSafeLinkUrl, sanitizeEmailHtml } from './rich-html';

  interface Props {
    html: string;
    disabled?: boolean;
    onchange?: (html: string) => void;
  }

  let { html, disabled = false, onchange }: Props = $props();

  let frameEl = $state<HTMLIFrameElement | null>(null);
  let loadedHtml = $state<string | null>(null);
  let lastEmitted = $state<string | null>(null);
  let serializedHtml = $state<string | null>(null);
  let srcdoc = $state('');
  let removeFrameListeners: (() => void) | null = null;
  let resizeObserver: ResizeObserver | null = null;
  let savedRange: Range | null = null;

  /** Commit exactly the editable root, never the iframe document or wrappers. */
  export function flush(): string {
    const editor = frameEl?.contentDocument?.getElementById('env-editor');
    if (!editor) return html;
    const next = sanitizeEmailHtml(editor.innerHTML, {
      remoteImages: true,
      externalLinkTargets: false
    }).html;
    if (next !== serializedHtml) {
      serializedHtml = next;
      lastEmitted = next;
      onchange?.(next);
    }
    fitToContent(editor);
    return next;
  }

  // Parent state follows every rich edit. Do not feed that value back through
  // srcdoc: replacing an iframe document on every keystroke destroys selection
  // and resets the cursor. Only genuinely external replacements (for example a
  // canonical body returned by Save) rebuild the document.
  $effect(() => {
    const next = html;
    if (next === loadedHtml) return;
    if (next === lastEmitted) {
      loadedHtml = next;
      lastEmitted = null;
      return;
    }
    loadedHtml = next;
    lastEmitted = null;
    srcdoc = buildSrcdoc(next);
  });

  $effect(() => {
    void srcdoc;
    return () => {
      clearFrameListeners();
      resizeObserver?.disconnect();
    };
  });

  $effect(() => {
    void disabled;
    const editor = frameEl?.contentDocument?.getElementById('env-editor');
    if (editor) editor.setAttribute('contenteditable', disabled ? 'false' : 'true');
  });

  function buildSrcdoc(rawHtml: string): string {
    const sanitized = sanitizeEmailHtml(rawHtml, {
      remoteImages: true,
      externalLinkTargets: false
    }).html;
    serializedHtml = sanitized;
    const inertHtml = sanitizeEmailHtml(sanitized, {
      remoteImages: true,
      externalLinkTargets: false,
      inertImages: true,
      inertLinks: true
    }).html;
    const csp =
      "default-src 'none'; script-src 'none'; connect-src 'none'; " +
      "style-src 'unsafe-inline'; img-src data: cid:; " +
      "font-src data:; frame-src 'none'; media-src 'none'; object-src 'none'; " +
      "form-action 'none'; base-uri 'none';";

    return (
      '<!doctype html><html><head>' +
      '<meta charset="utf-8">' +
      `<meta http-equiv="Content-Security-Policy" content="${csp}">` +
      '<style>' +
      "html,body{margin:0;background:#fff;color:#0a0a0a;font:14px/1.6 'Instrument Sans',system-ui,sans-serif;overflow-wrap:anywhere;}" +
      'body{padding:16px;}#env-editor{min-height:224px;outline:0;}#env-editor:focus{box-shadow:inset 2px 0 0 #1a6b4a;padding-left:10px;}' +
      'img{max-width:100%;height:auto;}a{color:#1a6b4a;}blockquote{border-left:3px solid #d9d7d1;margin:.75em 0;padding-left:.85em;color:#525252;}' +
      'table{max-width:100%;border-collapse:collapse;}pre{white-space:pre-wrap;font-size:13px;}' +
      '</style></head><body>' +
      `<div id="env-editor" role="textbox" aria-label="Message HTML body" aria-multiline="true" contenteditable="${disabled ? 'false' : 'true'}" spellcheck="true">${inertHtml}</div>` +
      '</body></html>'
    );
  }

  function onLoad(): void {
    clearFrameListeners();
    resizeObserver?.disconnect();
    const doc = frameEl?.contentDocument;
    const editor = doc?.getElementById('env-editor');
    if (!doc || !editor) return;

    const removeScrollBridge = frameEl ? installBodyFrameScrollBridge(frameEl) : () => {};
    editor.setAttribute('contenteditable', disabled ? 'false' : 'true');
    savedRange = null;

    const captureSelection = () => {
      const selection = doc.getSelection();
      if (!selection || selection.rangeCount === 0) return;
      const range = selection.getRangeAt(0);
      if (editor.contains(range.commonAncestorContainer)) savedRange = range.cloneRange();
    };
    const emit = () => {
      if (!disabled) flush();
      captureSelection();
    };
    const stopNavigation = (event: Event) => {
      const target = event.target as Element | null;
      if (target?.closest?.('a')) event.preventDefault();
    };
    const stopSubmit = (event: Event) => event.preventDefault();
    const paste = (event: ClipboardEvent) => insertTransfer(event, event.clipboardData);
    const drop = (event: DragEvent) => insertTransfer(event, event.dataTransfer);

    editor.addEventListener('input', emit);
    editor.addEventListener('keyup', captureSelection);
    editor.addEventListener('mouseup', captureSelection);
    doc.addEventListener('selectionchange', captureSelection);
    editor.addEventListener('click', stopNavigation);
    editor.addEventListener('auxclick', stopNavigation);
    editor.addEventListener('submit', stopSubmit);
    editor.addEventListener('paste', paste);
    editor.addEventListener('drop', drop);
    removeFrameListeners = () => {
      editor.removeEventListener('input', emit);
      editor.removeEventListener('keyup', captureSelection);
      editor.removeEventListener('mouseup', captureSelection);
      doc.removeEventListener('selectionchange', captureSelection);
      editor.removeEventListener('click', stopNavigation);
      editor.removeEventListener('auxclick', stopNavigation);
      editor.removeEventListener('submit', stopSubmit);
      editor.removeEventListener('paste', paste);
      editor.removeEventListener('drop', drop);
      removeScrollBridge();
      savedRange = null;
    };
    fitToContent(editor);
    if (typeof ResizeObserver !== 'undefined') {
      resizeObserver = new ResizeObserver(() => fitToContent(editor));
      resizeObserver.observe(editor);
    }
  }

  function clearFrameListeners(): void {
    removeFrameListeners?.();
    removeFrameListeners = null;
  }

  function fitToContent(editor: HTMLElement): void {
    if (!frameEl) return;
    const next = Math.max(256, editor.scrollHeight + 32);
    if (frameEl.style.height !== `${next}px`) frameEl.style.height = `${next}px`;
  }

  /** Sanitize clipboard/drop HTML before it enters the editable document. */
  function insertTransfer(event: ClipboardEvent | DragEvent, transfer: DataTransfer | null): void {
    event.preventDefault();
    if (disabled || !transfer) return;
    const doc = frameEl?.contentDocument;
    const selection = doc?.getSelection();
    if (!doc || !selection || selection.rangeCount === 0) return;

    const range = selection.getRangeAt(0);
    range.deleteContents();
    const transferredHtml = transfer.getData('text/html');
    let last: Node;
    if (transferredHtml) {
      const clean = sanitizeEmailHtml(transferredHtml, {
        remoteImages: true,
        externalLinkTargets: false,
        inertImages: true,
        inertLinks: true
      }).html;
      const fragment = range.createContextualFragment(clean);
      const candidate = fragment.lastChild;
      if (candidate) {
        last = candidate;
        range.insertNode(fragment);
      } else {
        last = doc.createTextNode('');
        range.insertNode(last);
      }
    } else {
      const fragment = doc.createDocumentFragment();
      const lines = transfer.getData('text/plain').replace(/\r\n?/g, '\n').split('\n');
      lines.forEach((line, index) => {
        if (index > 0) fragment.appendChild(doc.createElement('br'));
        fragment.appendChild(doc.createTextNode(line));
      });
      last = fragment.lastChild ?? doc.createTextNode('');
      range.insertNode(fragment.childNodes.length > 0 ? fragment : last);
    }
    range.setStartAfter(last);
    range.collapse(true);
    selection.removeAllRanges();
    selection.addRange(range);
    flush();
  }

  function editorDocument(): (Document & { execCommand?: (command: string, showUi?: boolean, value?: string) => boolean }) | null {
    return frameEl?.contentDocument ?? null;
  }

  function runCommand(command: string, value?: string): void {
    if (disabled) return;
    const doc = editorDocument();
    const editor = doc?.getElementById('env-editor');
    if (!doc || !editor || typeof doc.execCommand !== 'function') return;
    editor.focus();
    const selection = doc.getSelection();
    if (selection && savedRange && editor.contains(savedRange.commonAncestorContainer)) {
      selection.removeAllRanges();
      selection.addRange(savedRange.cloneRange());
    }
    doc.execCommand(command, false, value);
    const updatedSelection = doc.getSelection();
    if (updatedSelection && updatedSelection.rangeCount > 0) {
      const updated = updatedSelection.getRangeAt(0);
      if (editor.contains(updated.commonAncestorContainer)) savedRange = updated.cloneRange();
    }
    makeLinksInert(editor);
    flush();
  }

  /** An editor anchor carries its destination as data, so no gesture can navigate. */
  function makeLinksInert(editor: HTMLElement): void {
    editor.querySelectorAll('a[href]').forEach((link) => {
      const href = link.getAttribute('href') || '';
      link.removeAttribute('href');
      link.removeAttribute('target');
      link.removeAttribute('rel');
      if (isSafeLinkUrl(href)) link.setAttribute('data-env-href', href);
      else link.removeAttribute('data-env-href');
      link.setAttribute('tabindex', '-1');
    });
  }

  function insertLink(): void {
    const url = window.prompt('Link URL (https, http, mailto, or tel)');
    if (!url || !isSafeLinkUrl(url) || url.trim().startsWith('#')) return;
    runCommand('createLink', url.trim());
  }

  function clearFormatting(): void {
    runCommand('removeFormat');
    runCommand('unlink');
  }

  function keepSelection(event: MouseEvent): void {
    event.preventDefault();
  }
</script>

<div class="rich-editor">
  <div class="rich-toolbar" role="toolbar" aria-label="Rich text formatting">
    <button type="button" aria-label="Bold" title="Bold" {disabled} onmousedown={keepSelection} onclick={() => runCommand('bold')}><strong>B</strong></button>
    <button type="button" aria-label="Italic" title="Italic" {disabled} onmousedown={keepSelection} onclick={() => runCommand('italic')}><em>I</em></button>
    <button type="button" aria-label="Underline" title="Underline" {disabled} onmousedown={keepSelection} onclick={() => runCommand('underline')}><span class="underline">U</span></button>
    <span class="toolbar-separator" aria-hidden="true"></span>
    <button type="button" aria-label="Insert link" title="Insert link" {disabled} onmousedown={keepSelection} onclick={insertLink}>Link</button>
    <button type="button" aria-label="Ordered list" title="Ordered list" {disabled} onmousedown={keepSelection} onclick={() => runCommand('insertOrderedList')}>1.</button>
    <button type="button" aria-label="Unordered list" title="Unordered list" {disabled} onmousedown={keepSelection} onclick={() => runCommand('insertUnorderedList')}>•</button>
    <span class="toolbar-separator" aria-hidden="true"></span>
    <button type="button" aria-label="Clear formatting" title="Clear formatting" {disabled} onmousedown={keepSelection} onclick={clearFormatting}>Clear</button>
  </div>
  <iframe
    bind:this={frameEl}
    class="rich-frame"
    title="Rich text message editor"
    sandbox="allow-same-origin"
    {srcdoc}
    onload={onLoad}
  ></iframe>
</div>

<style>
  .rich-editor {
    min-height: 16rem;
    background: var(--env-surface);
  }
  .rich-toolbar {
    min-height: 2.35rem;
    display: flex;
    align-items: center;
    gap: 0.2rem;
    padding: 0.25rem 0.55rem;
    border-bottom: 1px solid var(--env-rule);
    background: var(--env-surface);
  }
  .rich-toolbar button {
    min-width: 1.9rem;
    min-height: 1.8rem;
    padding: 0.2rem 0.45rem;
    border: 1px solid transparent;
    border-radius: 0.2rem;
    background: transparent;
    color: var(--env-ink);
    cursor: pointer;
    font-family: var(--font-mono);
    font-size: 0.72rem;
  }
  .rich-toolbar button:hover,
  .rich-toolbar button:focus-visible {
    border-color: var(--env-rule);
    background: var(--env-paper);
    outline: 0;
  }
  .rich-toolbar button:disabled {
    cursor: not-allowed;
    opacity: 0.45;
  }
  .underline {
    text-decoration: underline;
  }
  .toolbar-separator {
    width: 1px;
    height: 1.2rem;
    margin: 0 0.2rem;
    background: var(--env-rule);
  }
  .rich-frame {
    display: block;
    width: 100%;
    min-height: 16rem;
    height: 16rem;
    border: 0;
    background: var(--env-surface);
  }
</style>
