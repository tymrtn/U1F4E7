<script lang="ts">
  import { installBodyFrameScrollBridge } from './body-frame-scroll';
  import { resolveFrameHeight } from './bodyframe-size';
  import { sanitizeEmailHtml } from './rich-html';

  // BodyFrame — renders an HTML email body inside a sandboxed <iframe srcdoc>.
  //
  // Sandbox design (mirrors and tightens v1 dashboard.js):
  //
  //   v1 used: sandbox="allow-same-origin" only.
  //     Rationale: allow-same-origin lets the parent JS read the rendered
  //     content height to auto-size the frame. Scripts are never enabled.
  //     v1 sanitizes the HTML first (stripDangerousEmailNodes removes all
  //     <script>, event handlers, dangerous URLs). The combination is safe
  //     because allow-scripts is never present.
  //
  //   v2 uses: the same sandbox="allow-same-origin" posture.
  //     We omit allow-scripts entirely. allow-same-origin is kept so
  //     ResizeObserver on the parent can measure contentDocument.scrollHeight
  //     for auto-sizing. This equals v1 security: the only mechanism that
  //     could abuse allow-same-origin is JS running inside the frame, and
  //     allow-scripts is absent.
  //
  //   CSP injected into srcdoc:
  //     default-src 'none'; style-src 'unsafe-inline'; img-src <mode>;
  //     font-src data:; frame-src 'none';
  //   When remoteImages=false:  img-src data: cid:  (no https:)
  //   When remoteImages=true:   img-src data: cid: https:
  //
  //   The CSP lives in a <meta http-equiv="Content-Security-Policy"> inside
  //   srcdoc, which browsers honour for same-origin iframes even without an
  //   HTTP header. This is defence-in-depth on top of the sandbox attribute.
  //
  //   external links get target="_blank" rel="noopener noreferrer" via the
  //   base element and per-link injection in the sanitizer.

  interface Props {
    /** Raw HTML email body. Never injected as {@html} into the parent DOM. */
    html: string;
    /** When true, https: image sources are permitted by the CSP. */
    remoteImages?: boolean;
    /** Called with the number of remote images that were blocked (>=1 = show button). */
    onRemoteBlocked?: (count: number) => void;
  }

  let { html, remoteImages = false, onRemoteBlocked }: Props = $props();

  let frameEl = $state<HTMLIFrameElement | null>(null);
  let resizeObserver: ResizeObserver | null = null;
  let removeFrameListeners: (() => void) | null = null;

  /** Sanitize an HTML email string and wrap it in a safe document.
   *  Returns { srcdoc, remoteBlocked } — side-effects (callback) stay in $effect. */
  function buildSrcdoc(rawHtml: string, allowRemote: boolean): { srcdoc: string; remoteBlocked: number } {
    // Sanitize first, then parse that inert body to apply reader-only quote
    // collapsing. Keeping the sanitizer shared with the rich draft editor
    // prevents the editable and read-only HTML boundaries from drifting.
    const sanitized = sanitizeEmailHtml(rawHtml, { remoteImages: allowRemote });
    const doc = new DOMParser().parseFromString(sanitized.html, 'text/html');

    // Collapse quoted replies with <details> (no JS needed).
    collapseQuotes(doc);

    const imgSrc = allowRemote ? "data: cid: https:" : "data: cid:";
    const csp =
      `default-src 'none'; script-src 'none'; connect-src 'none'; ` +
      `style-src 'unsafe-inline'; img-src ${imgSrc}; ` +
      `font-src data:; frame-src 'none'; media-src 'none'; object-src 'none'; ` +
      `form-action 'none'; base-uri 'none';`;

    const body = doc.body ? doc.body.innerHTML : '';
    const srcdoc = `<!doctype html><html><head>` +
      `<meta charset="utf-8">` +
      `<meta http-equiv="Content-Security-Policy" content="${csp}">` +
      `<style>` +
      `html,body{margin:0;padding:0 1px;background:#f7f6f3;color:#0a0a0a;` +
      `font:14px/1.6 'Instrument Sans',system-ui,sans-serif;overflow-wrap:anywhere;}` +
      `img{max-width:100%;height:auto;}` +
      `a{color:#1a6b4a;}` +
      `blockquote{border-left:3px solid #d9d7d1;margin:.75em 0;padding-left:.85em;color:#525252;}` +
      `table{max-width:100%;border-collapse:collapse;}` +
      `pre{white-space:pre-wrap;font-size:13px;}` +
      `details.env-quote{margin:.75em 0;}` +
      `summary.env-quote-toggle{cursor:pointer;color:#1a6b4a;font-size:13px;` +
      `list-style:none;user-select:none;padding:2px 0;}` +
      `summary.env-quote-toggle::-webkit-details-marker{display:none;}` +
      `#env-content{display:block;}` +
      // A block wrapper whose height IS the content height, measurable without
      // collapsing the iframe. documentElement.scrollHeight is at least the
      // viewport height, which is why the previous sizer had to collapse first.
      `</style></head><body><div id="env-content">${body}</div></body></html>`;

    return { srcdoc, remoteBlocked: sanitized.remoteBlocked };
  }

  function collapseQuotes(doc: Document): void {
    const body = doc.body;
    if (!body) return;
    const candidates = Array.from(
      body.querySelectorAll('blockquote, .gmail_quote, div[class*="gmail_quote"]')
    );
    for (const node of candidates) {
      if (!node.parentNode) continue;
      if (candidates.some((other) => other !== node && other.contains(node))) continue;
      // Only collapse if there's real content before the quote.
      if (!hasMeaningfulContentBefore(node)) continue;
      const details = doc.createElement('details');
      details.className = 'env-quote';
      const summary = doc.createElement('summary');
      summary.className = 'env-quote-toggle';
      summary.textContent = 'Show quoted text';
      node.parentNode.insertBefore(details, node);
      details.appendChild(summary);
      details.appendChild(node);
    }
  }

  function hasMeaningfulContentBefore(node: Element): boolean {
    let cur: Node | null = node;
    while (cur && cur.parentNode) {
      let sib: Node | null = cur.previousSibling;
      while (sib) {
        if (sib.nodeType === 3 && (sib.textContent || '').replace(/\s+/g, '').length > 0) return true;
        if (sib.nodeType === 1 && (sib.textContent || '').replace(/\s+/g, '').length > 0) return true;
        sib = sib.previousSibling;
      }
      cur = cur.parentNode;
      if (cur && (cur as Element).tagName?.toLowerCase() === 'body') break;
    }
    return false;
  }

  /**
   * Measure the rendered document and size the frame to it.
   *
   * Two rules, both load-bearing, both learned the hard way:
   *
   * 1. Never measure against a collapsed frame.
   *    `documentElement.scrollHeight` is at least the frame's own viewport
   *    height, so an earlier sizer set the frame to 0px to get a true reading
   *    — and the frame lives inside the reader's scroll container, so every
   *    collapse shrank that container, the browser clamped `scrollTop`, and
   *    the pane snapped back to the top. Measuring the `#env-content` wrapper
   *    gives the content height at any frame height, so nothing is collapsed.
   *
   * 2. Only write when the height actually changed. The observer watches the
   *    content, so writing a height on every callback keeps waking it. A
   *    settled frame writes nothing and the observer goes quiet.
   *
   * The height stays uncapped: a capped iframe turns the remainder into an
   * inner scroll surface and makes the end of a tall email unreachable.
   */
  function fitToContent() {
    if (!frameEl) return;
    try {
      const wrapper = frameEl.contentDocument?.getElementById('env-content');
      if (!wrapper) return;
      const next = resolveFrameHeight(
        wrapper.getBoundingClientRect().height,
        frameEl.getBoundingClientRect().height,
        undefined,
        Number.POSITIVE_INFINITY
      );
      if (next !== null) frameEl.style.height = `${next}px`;
    } catch {
      // Cross-origin or not yet ready — leave the current height.
    }
  }

  /**
   * Size the frame after load, and keep sizing it.
   *
   * A single measurement at load is wrong for almost every real email: images
   * (remote ones especially) arrive after the document fires `load`, custom
   * fonts reflow the text, and opening a collapsed quote changes the height
   * long afterwards. Measuring once left the frame short and the message
   * clipped, which the old fixed height then hid behind an inner scrollbar.
   * Observe the content instead and follow it.
   */
  function onLoad() {
    if (!frameEl) return;
    clearFrameListeners();
    resizeObserver?.disconnect();
    const doc = frameEl.contentDocument;
    if (!doc) return;

    const listenerCleanups = [installBodyFrameScrollBridge(frameEl)];

    fitToContent();

    // Observe the content wrapper, not documentElement or body:
    // documentElement's box tracks the iframe viewport, so observing it feeds
    // every height we write straight back in as another callback. The observer
    // is attached once and left alone — disconnecting and re-observing inside
    // the callback made `observe()` deliver its guaranteed initial callback
    // again, which is a loop that never settles.
    const wrapper = doc.getElementById('env-content');
    if (wrapper && typeof ResizeObserver !== 'undefined') {
      resizeObserver = new ResizeObserver(fitToContent);
      resizeObserver.observe(wrapper);
    }

    // Late-arriving images and fonts.
    for (const img of Array.from(doc.images)) {
      if (!img.complete) {
        img.addEventListener('load', fitToContent, { once: true });
        img.addEventListener('error', fitToContent, { once: true });
        listenerCleanups.push(() => {
          img.removeEventListener('load', fitToContent);
          img.removeEventListener('error', fitToContent);
        });
      }
    }

    removeFrameListeners = () => listenerCleanups.forEach((cleanup) => cleanup());
  }

  function clearFrameListeners(): void {
    removeFrameListeners?.();
    removeFrameListeners = null;
  }

  // Build the srcdoc as a derived value (pure computation — no side effects).
  let built = $derived(buildSrcdoc(html, remoteImages));
  let srcdoc = $derived(built.srcdoc);

  // Replacing srcdoc replaces the document. Detach listeners from the old
  // document immediately, and perform the same cleanup on component teardown.
  $effect(() => {
    void srcdoc;
    return () => {
      clearFrameListeners();
      resizeObserver?.disconnect();
    };
  });

  // Notify the parent of blocked-image count after each build.
  // $effect is the right place for callbacks that touch external state.
  $effect(() => {
    onRemoteBlocked?.(built.remoteBlocked);
  });
</script>

<iframe
  bind:this={frameEl}
  class="body-frame"
  id="body-frame"
  title="Message body"
  sandbox="allow-same-origin"
  {srcdoc}
  onload={onLoad}
></iframe>

<style>
  .body-frame {
    width: 100%;
    /* A starting height only — `fitToContent` replaces it with the rendered
       document height as soon as the frame loads, and again whenever the
       content reflows. The frame never scrolls internally; the page does. */
    min-height: 8rem;
    height: 20rem;
    border: none;
    background: var(--env-paper);
    display: block;
  }

</style>
