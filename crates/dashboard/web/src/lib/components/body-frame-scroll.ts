const AXIS_LOCK_THRESHOLD_PX = 4;
const WHEEL_LINE_HEIGHT_PX = 16;

type TouchAxis = 'pending' | 'vertical' | 'native';

interface TouchTrack {
  identifier: number;
  startX: number;
  startY: number;
  lastY: number;
  axis: TouchAxis;
}

/**
 * Hand vertical scroll gestures from a same-origin message iframe to the
 * reader surface that owns scrolling.
 *
 * Events do not bubble across an iframe boundary, even when the iframe is
 * same-origin. Installing capture listeners on the loaded srcdoc lets the
 * parent cancel the iframe's default scroll and apply the same delta to the
 * closest real parent scroller. Clicks, selections, horizontal gestures, and
 * multi-touch gestures are deliberately left alone.
 */
export function installBodyFrameScrollBridge(frame: HTMLIFrameElement): () => void {
  const contentDocument = frame.contentDocument;
  if (!contentDocument) return () => {};

  let touchTrack: TouchTrack | null = null;

  const onWheel = (event: WheelEvent) => {
    // ctrl+wheel is commonly pinch-to-zoom, and shift+wheel is horizontal.
    if (!event.cancelable || event.ctrlKey || event.shiftKey) return;
    if (Math.abs(event.deltaY) <= Math.abs(event.deltaX) || event.deltaY === 0) return;

    const scrollOwner = findScrollOwner(frame);
    if (!scrollOwner) return;

    event.preventDefault();
    scrollOwner.scrollTop += normalizedWheelY(event, scrollOwner);
  };

  const onTouchStart = (event: TouchEvent) => {
    if (event.touches.length !== 1) {
      touchTrack = null;
      return;
    }

    const touch = event.touches[0];
    touchTrack = {
      identifier: touch.identifier,
      startX: touch.clientX,
      startY: touch.clientY,
      lastY: touch.clientY,
      axis: hasActiveSelection(contentDocument) ? 'native' : 'pending'
    };
  };

  const onTouchMove = (event: TouchEvent) => {
    if (!touchTrack || event.touches.length !== 1) {
      touchTrack = null;
      return;
    }

    const touch = Array.from(event.touches).find(
      (candidate) => candidate.identifier === touchTrack?.identifier
    );
    if (!touch) {
      touchTrack = null;
      return;
    }

    const deltaY = touchTrack.lastY - touch.clientY;
    touchTrack.lastY = touch.clientY;

    if (touchTrack.axis === 'native') return;
    if (hasActiveSelection(contentDocument)) {
      touchTrack.axis = 'native';
      return;
    }

    if (touchTrack.axis === 'pending') {
      const totalX = touchTrack.startX - touch.clientX;
      const totalY = touchTrack.startY - touch.clientY;
      if (Math.max(Math.abs(totalX), Math.abs(totalY)) < AXIS_LOCK_THRESHOLD_PX) return;

      // Once a gesture looks horizontal, leave the whole gesture native. This
      // keeps wide email content and browser back/forward gestures usable.
      if (Math.abs(totalY) <= Math.abs(totalX)) {
        touchTrack.axis = 'native';
        return;
      }
      touchTrack.axis = 'vertical';
    }

    if (!event.cancelable || deltaY === 0) return;
    const scrollOwner = findScrollOwner(frame);
    if (!scrollOwner) return;

    event.preventDefault();
    scrollOwner.scrollTop += deltaY;
  };

  const clearTouch = () => {
    touchTrack = null;
  };

  contentDocument.addEventListener('wheel', onWheel, { capture: true, passive: false });
  contentDocument.addEventListener('touchstart', onTouchStart, { capture: true, passive: true });
  contentDocument.addEventListener('touchmove', onTouchMove, { capture: true, passive: false });
  contentDocument.addEventListener('touchend', clearTouch, { capture: true, passive: true });
  contentDocument.addEventListener('touchcancel', clearTouch, { capture: true, passive: true });

  return () => {
    contentDocument.removeEventListener('wheel', onWheel, true);
    contentDocument.removeEventListener('touchstart', onTouchStart, true);
    contentDocument.removeEventListener('touchmove', onTouchMove, true);
    contentDocument.removeEventListener('touchend', clearTouch, true);
    contentDocument.removeEventListener('touchcancel', clearTouch, true);
  };
}

function findScrollOwner(frame: HTMLIFrameElement): HTMLElement | null {
  const ownerDocument = frame.ownerDocument;
  const view = ownerDocument.defaultView;

  for (let node = frame.parentElement; node; node = node.parentElement) {
    if (!view) break;
    const style = view.getComputedStyle(node);
    const overflowY = style.overflowY || style.overflow;
    if (/^(auto|scroll)$/.test(overflowY) && node.scrollHeight > node.clientHeight) {
      return node;
    }
  }

  const documentScroller = ownerDocument.scrollingElement;
  if (documentScroller instanceof HTMLElement) return documentScroller;
  return ownerDocument.documentElement;
}

function normalizedWheelY(event: WheelEvent, scrollOwner: HTMLElement): number {
  if (event.deltaMode === 1) return event.deltaY * WHEEL_LINE_HEIGHT_PX;
  if (event.deltaMode === 2) {
    const pageHeight = scrollOwner.clientHeight || scrollOwner.ownerDocument.defaultView?.innerHeight;
    return event.deltaY * (pageHeight || 1);
  }
  return event.deltaY;
}

function hasActiveSelection(doc: Document): boolean {
  const selection = typeof doc.getSelection === 'function' ? doc.getSelection() : null;
  return Boolean(selection && !selection.isCollapsed);
}
