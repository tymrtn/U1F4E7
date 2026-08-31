<script lang="ts">
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

  /** Sanitize an HTML email string and wrap it in a safe document.
   *  Returns { srcdoc, remoteBlocked } — side-effects (callback) stay in $effect. */
  function buildSrcdoc(rawHtml: string, allowRemote: boolean): { srcdoc: string; remoteBlocked: number } {
    // ── Sanitize inside a detached document ──
    const parser = new DOMParser();
    const doc = parser.parseFromString(rawHtml, 'text/html');

    // 1. Strip dangerous elements.
    doc
      .querySelectorAll(
        'script, link, form, input, button, textarea, select, iframe, object, embed, applet, meta, base'
      )
      .forEach((el) => el.remove());

    // 2. Strip dangerous attributes on every element.
    doc.querySelectorAll('*').forEach((el) => {
      for (const attr of Array.from(el.attributes)) {
        const name = attr.name.toLowerCase();
        const value = attr.value || '';

        // Event handlers
        if (name.startsWith('on')) {
          el.removeAttribute(attr.name);
          continue;
        }
        // background attr (old HTML)
        if (name === 'background') {
          el.removeAttribute(attr.name);
          continue;
        }
        // srcset on anything
        if (name === 'srcset') {
          el.removeAttribute(attr.name);
          continue;
        }
        // javascript: / data:text/html hrefs
        if ((name === 'href' || name === 'xlink:href') && isDangerousUrl(value)) {
          el.removeAttribute(attr.name);
          continue;
        }
        // CSS url() / @import in style attrs (tracking pixels / remote fonts)
        if (name === 'style' && hasCssUrlLoad(value)) {
          el.removeAttribute(attr.name);
          continue;
        }
      }
      // External links open in a new tab with noopener.
      const tag = el.tagName.toLowerCase();
      if (tag === 'a') {
        const href = el.getAttribute('href') || '';
        if (/^https?:\/\//i.test(href)) {
          el.setAttribute('target', '_blank');
          el.setAttribute('rel', 'noopener noreferrer');
        }
      }
    });

    // 3. Handle images: count remote ones; block or permit per remoteImages flag.
    let remoteBlocked = 0;
    doc.querySelectorAll('img').forEach((img) => {
      const src = (img.getAttribute('src') || '').trim();
      img.removeAttribute('srcset');
      if (/^https?:\/\//i.test(src)) {
        if (!allowRemote) {
          img.setAttribute('data-remote-src', src);
          img.setAttribute('src', transparentDataUrl());
          img.setAttribute('alt', img.getAttribute('alt') || 'Remote image (blocked)');
          img.setAttribute('title', 'Remote images are blocked');
          remoteBlocked += 1;
        }
        // If allowRemote, leave the src untouched.
        return;
      }
      // Block other non-inline non-data sources.
      if (src && !/^data:/i.test(src) && !/^cid:/i.test(src)) {
        img.removeAttribute('src');
      }
    });

    // Collapse quoted replies with <details> (no JS needed).
    collapseQuotes(doc);

    const imgSrc = allowRemote ? "data: cid: https:" : "data: cid:";
    const csp =
      `default-src 'none'; style-src 'unsafe-inline'; img-src ${imgSrc}; ` +
      `font-src data:; frame-src 'none'; media-src 'none'; object-src 'none';`;

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
      `</style></head><body>${body}</body></html>`;

    return { srcdoc, remoteBlocked };
  }

  function isDangerousUrl(value: string): boolean {
    const t = value.trim();
    return /^javascript:/i.test(t) || /^data:text\/html/i.test(t);
  }

  function hasCssUrlLoad(value: string): boolean {
    return /@import\b/i.test(value) || /url\s*\(/i.test(value);
  }

  function transparentDataUrl(): string {
    return 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%221%22 height=%221%22/%3E';
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
   * The frame is collapsed before measuring. `documentElement.scrollHeight`
   * cannot report less than the frame's own viewport, so measuring while the
   * frame is already tall returns the frame's height rather than the content's
   * — and with a ResizeObserver attached, feeding that back in makes the frame
   * grow on every pass (a 2,164px email measured 3,369px and climbing). Zero
   * height first, read, then set: the reading is always the content.
   */
  function fitToContent() {
    if (!frameEl) return;
    try {
      const doc = frameEl.contentDocument;
      if (!doc?.documentElement) return;
      // Don't observe our own measurement.
      resizeObserver?.disconnect();
      frameEl.style.height = '0px';
      const height = doc.documentElement.scrollHeight;
      if (height > 0) frameEl.style.height = `${Math.min(height + 16, 20000)}px`;
      if (doc.body && resizeObserver) resizeObserver.observe(doc.body);
    } catch {
      // Cross-origin or not yet ready — leave the fallback height.
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
    const doc = frameEl.contentDocument;
    if (!doc) return;

    // Create the observer before the first measurement so `fitToContent` can
    // suspend it around its own writes.
    if (doc.body && typeof ResizeObserver !== 'undefined' && !resizeObserver) {
      resizeObserver = new ResizeObserver(fitToContent);
    }
    fitToContent();

    // Late-arriving images and fonts.
    for (const img of Array.from(doc.images)) {
      if (!img.complete) {
        img.addEventListener('load', fitToContent, { once: true });
        img.addEventListener('error', fitToContent, { once: true });
      }
    }

  }

  $effect(() => {
    return () => resizeObserver?.disconnect();
  });

  // Build the srcdoc as a derived value (pure computation — no side effects).
  let built = $derived(buildSrcdoc(html, remoteImages));
  let srcdoc = $derived(built.srcdoc);

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

  /* One-column mobile layout (same 760px breakpoint the panes collapse at):
     the document owns scrolling, but iOS keeps a vertical touch that STARTS
     inside the sandboxed iframe captive in the frame's context — a tall
     forwarded message can never be flicked past to the attachments and
     Human-only Send controls under it. Refuse the gesture at the frame so it
     lands on the document scroller. Tradeoff, accepted for reachable
     controls: the preview is read-only on narrow screens (no link taps or
     quote toggles). The frame stays visible, focusable, and in the
     accessibility tree. */
  @media (max-width: 760px) {
    .body-frame {
      pointer-events: none;
    }
  }
</style>
