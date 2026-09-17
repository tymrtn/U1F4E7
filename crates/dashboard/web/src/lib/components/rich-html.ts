export interface SanitizeEmailHtmlOptions {
  remoteImages?: boolean;
  externalLinkTargets?: boolean;
  /** Replace remote image src values with inert data attributes inside an editor frame. */
  inertImages?: boolean;
  /** Replace anchor hrefs with inert data attributes inside an editor frame. */
  inertLinks?: boolean;
}

export interface SanitizedEmailHtml {
  html: string;
  remoteBlocked: number;
}

const DANGEROUS_ELEMENTS =
  'script, style, link, form, input, button, textarea, select, option, iframe, frame, frameset, object, embed, applet, meta, base, template, area, audio, video, source, track, canvas, svg, math';

/**
 * Sanitize email HTML in a detached document before it reaches any iframe.
 *
 * The iframe sandbox and CSP remain the primary execution boundary; this pass
 * removes active content and dangerous navigation/load attributes as a second
 * independent layer. The returned string contains body markup only.
 */
export function sanitizeEmailHtml(
  rawHtml: string,
  options: SanitizeEmailHtmlOptions = {}
): SanitizedEmailHtml {
  const {
    remoteImages = false,
    externalLinkTargets = true,
    inertImages = false,
    inertLinks = false
  } = options;
  const doc = new DOMParser().parseFromString(rawHtml, 'text/html');

  doc.querySelectorAll(DANGEROUS_ELEMENTS).forEach((element) => element.remove());

  doc.querySelectorAll('*').forEach((element) => {
    for (const attribute of Array.from(element.attributes)) {
      const name = attribute.name.toLowerCase();
      const value = attribute.value || '';

      if (
        name.startsWith('on') ||
        name === 'background' ||
        name === 'srcset' ||
        name === 'srcdoc' ||
        name === 'action' ||
        name === 'formaction' ||
        name === 'poster' ||
        name === 'ping' ||
        name === 'download' ||
        name === 'contenteditable' ||
        name === 'autofocus' ||
        name === 'tabindex'
      ) {
        element.removeAttribute(attribute.name);
        continue;
      }

      if (name === 'xlink:href') {
        element.removeAttribute(attribute.name);
        continue;
      }

      if (name === 'href' && !isSafeLinkUrl(value)) {
        element.removeAttribute(attribute.name);
        continue;
      }

      if (name === 'style' && hasDangerousCss(value)) {
        element.removeAttribute(attribute.name);
      }
    }
  });

  let remoteBlocked = 0;
  doc.querySelectorAll('img').forEach((image) => {
    const src = (
      image.getAttribute('data-env-src') ||
      image.getAttribute('data-remote-src') ||
      image.getAttribute('src') ||
      ''
    ).trim();
    image.removeAttribute('src');
    image.removeAttribute('srcset');
    image.removeAttribute('data-env-src');
    image.removeAttribute('data-remote-src');

    if (isRemoteImageUrl(src)) {
      if (inertImages) {
        image.setAttribute('data-env-src', src);
        image.setAttribute('alt', image.getAttribute('alt') || 'Remote image');
      } else if (!remoteImages) {
        image.setAttribute('data-remote-src', src);
        image.setAttribute('src', transparentDataUrl());
        image.setAttribute('alt', image.getAttribute('alt') || 'Remote image (blocked)');
        image.setAttribute('title', 'Remote images are blocked');
        remoteBlocked += 1;
      } else {
        image.setAttribute('src', src);
      }
      return;
    }

    if (src && isSafeInlineImageUrl(src)) image.setAttribute('src', src);
  });

  doc.querySelectorAll('a').forEach((link) => {
    const href = link.getAttribute('href') || link.getAttribute('data-env-href') || '';
    link.removeAttribute('href');
    link.removeAttribute('data-env-href');
    link.removeAttribute('target');
    link.removeAttribute('rel');
    link.removeAttribute('tabindex');
    if (!isSafeLinkUrl(href)) return;
    if (inertLinks) {
      link.setAttribute('data-env-href', href);
      link.setAttribute('tabindex', '-1');
    } else {
      link.setAttribute('href', href);
    }
    if (!inertLinks && externalLinkTargets && isHttpUrl(href)) {
      link.setAttribute('target', '_blank');
      link.setAttribute('rel', 'noopener noreferrer');
    }
  });

  return { html: doc.body?.innerHTML ?? '', remoteBlocked };
}

/** Link schemes supported by outbound email without script or data execution. */
export function isSafeLinkUrl(value: string): boolean {
  const compact = normalizeUrl(value);
  if (/^#[^\s]*$/.test(compact)) return true;
  const protocol = parsedProtocol(compact);
  return protocol === 'http:' || protocol === 'https:' || protocol === 'mailto:' || protocol === 'tel:';
}

function isSafeInlineImageUrl(value: string): boolean {
  const compact = normalizeUrl(value);
  return /^cid:/i.test(compact) || /^data:image\/(?:png|gif|jpe?g|webp|avif)[;,]/i.test(compact);
}

function isRemoteImageUrl(value: string): boolean {
  const protocol = parsedProtocol(normalizeUrl(value));
  return protocol === 'http:' || protocol === 'https:';
}

function isHttpUrl(value: string): boolean {
  const protocol = parsedProtocol(normalizeUrl(value));
  return protocol === 'http:' || protocol === 'https:';
}

function normalizeUrl(value: string): string {
  // Browsers ignore assorted ASCII controls while parsing schemes. Remove the
  // same range before URL parsing so `java\nscript:` cannot evade policy.
  return value.trim().replace(/[\u0000-\u0020\u007f-\u009f]/g, '');
}

function parsedProtocol(value: string): string | null {
  if (!value || value.startsWith('#')) return null;
  try {
    return new URL(value).protocol.toLowerCase();
  } catch {
    return null;
  }
}

function hasDangerousCss(value: string): boolean {
  return (
    // CSS escapes can hide both function names and URL schemes (for example
    // `\69 mage-set("\68 \74 \74 \70 \73 ...")`). A mail client is not a CSS
    // parser; reject escaped inline CSS rather than attempting an incomplete
    // decoder that browsers may interpret differently.
    /\\/.test(value) ||
    /@import\b/i.test(value) ||
    /url\s*\(/i.test(value) ||
    /(?:-webkit-)?image-set\s*\(/i.test(value) ||
    /(?:https?|data|blob|file)\s*:/i.test(value) ||
    /expression\s*\(/i.test(value) ||
    /(?:behavior|-moz-binding)\s*:/i.test(value)
  );
}

export function transparentDataUrl(): string {
  return 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%221%22 height=%221%22/%3E';
}
