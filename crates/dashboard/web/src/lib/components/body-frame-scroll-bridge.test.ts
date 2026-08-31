import { afterEach, describe, expect, it } from 'vitest';

import { installBodyFrameScrollBridge } from './body-frame-scroll';

interface MockScrollState {
  get(): number;
  set(value: number): void;
  restore(): void;
}

const fixtures: HTMLElement[] = [];
const scrollMocks: MockScrollState[] = [];

afterEach(() => {
  for (const mock of scrollMocks.splice(0)) mock.restore();
  for (const fixture of fixtures.splice(0)) fixture.remove();
});

function frameInside(parent: HTMLElement): HTMLIFrameElement {
  const frame = document.createElement('iframe');
  parent.appendChild(frame);
  document.body.appendChild(parent);
  fixtures.push(parent);
  return frame;
}

function mockScrollable(
  element: HTMLElement,
  { initial = 0, maximum = 500 }: { initial?: number; maximum?: number } = {}
): MockScrollState {
  let scrollTop = initial;
  const originalDescriptors = new Map(
    ['clientHeight', 'scrollHeight', 'scrollTop'].map((property) => [
      property,
      Object.getOwnPropertyDescriptor(element, property)
    ])
  );

  Object.defineProperties(element, {
    clientHeight: { configurable: true, value: 100 },
    scrollHeight: { configurable: true, value: maximum + 100 },
    scrollTop: {
      configurable: true,
      get: () => scrollTop,
      set: (value: number) => {
        scrollTop = Math.max(0, Math.min(maximum, value));
      }
    }
  });

  const state = {
    get: () => scrollTop,
    set: (value: number) => {
      scrollTop = value;
    },
    restore: () => {
      for (const [property, descriptor] of originalDescriptors) {
        if (descriptor) Object.defineProperty(element, property, descriptor);
        else delete (element as unknown as Record<string, unknown>)[property];
      }
    }
  };
  scrollMocks.push(state);
  return state;
}

function dispatchWheel(
  frame: HTMLIFrameElement,
  { deltaX = 0, deltaY = 0 }: { deltaX?: number; deltaY?: number }
): WheelEvent {
  const event = new WheelEvent('wheel', { bubbles: true, cancelable: true, deltaX, deltaY });
  frame.contentDocument?.dispatchEvent(event);
  return event;
}

function dispatchTouch(
  frame: HTMLIFrameElement,
  type: 'touchstart' | 'touchmove' | 'touchend',
  points: Array<{ identifier: number; clientX: number; clientY: number }>
): TouchEvent {
  const event = new Event(type, { bubbles: true, cancelable: true }) as TouchEvent;
  Object.defineProperty(event, 'touches', {
    value: points.map((point) => point as Touch),
    configurable: true
  });
  frame.contentDocument?.dispatchEvent(event);
  return event;
}

describe('BodyFrame iframe scroll bridge', () => {
  it('forwards only vertical wheel input and never falls back to iframe scrolling', () => {
    const scrollOwner = document.createElement('div');
    scrollOwner.style.overflowY = 'auto';
    const state = mockScrollable(scrollOwner, { initial: 40 });
    const frame = frameInside(scrollOwner);
    const cleanup = installBodyFrameScrollBridge(frame);

    const vertical = dispatchWheel(frame, { deltaX: 2, deltaY: 35 });
    expect(state.get()).toBe(75);
    expect(vertical.defaultPrevented).toBe(true);

    const horizontal = dispatchWheel(frame, { deltaX: 40, deltaY: 5 });
    expect(state.get()).toBe(75);
    expect(horizontal.defaultPrevented).toBe(false);

    state.set(500);
    const atBoundary = dispatchWheel(frame, { deltaY: 20 });
    expect(state.get()).toBe(500);
    // Even at the outer boundary, do not let a nested overflow style inside
    // the email become a second scroll surface.
    expect(atBoundary.defaultPrevented).toBe(true);
    cleanup();
  });

  it('bridges vertical touch motion while leaving horizontal gestures native', () => {
    const scrollOwner = document.createElement('div');
    scrollOwner.style.overflowY = 'scroll';
    const state = mockScrollable(scrollOwner, { initial: 20 });
    const frame = frameInside(scrollOwner);
    const cleanup = installBodyFrameScrollBridge(frame);

    dispatchTouch(frame, 'touchstart', [{ identifier: 1, clientX: 50, clientY: 100 }]);
    const vertical = dispatchTouch(frame, 'touchmove', [
      { identifier: 1, clientX: 51, clientY: 70 }
    ]);
    expect(state.get()).toBe(50);
    expect(vertical.defaultPrevented).toBe(true);

    dispatchTouch(frame, 'touchend', []);
    dispatchTouch(frame, 'touchstart', [{ identifier: 2, clientX: 100, clientY: 100 }]);
    const horizontal = dispatchTouch(frame, 'touchmove', [
      { identifier: 2, clientX: 60, clientY: 96 }
    ]);
    expect(state.get()).toBe(50);
    expect(horizontal.defaultPrevented).toBe(false);
    cleanup();
  });

  it('falls back to the outer document scroller when no ancestor can scroll', () => {
    const wrapper = document.createElement('div');
    wrapper.style.overflowY = 'visible';
    const frame = frameInside(wrapper);
    const state = mockScrollable(document.documentElement, { initial: 10 });
    const cleanup = installBodyFrameScrollBridge(frame);

    const event = dispatchWheel(frame, { deltaY: 25 });
    expect(state.get()).toBe(35);
    expect(event.defaultPrevented).toBe(true);
    cleanup();
  });

  it('forwards mobile touch motion to the document scroller', () => {
    const wrapper = document.createElement('div');
    wrapper.style.overflowY = 'visible';
    const frame = frameInside(wrapper);
    const state = mockScrollable(document.documentElement, { initial: 60 });
    const cleanup = installBodyFrameScrollBridge(frame);

    dispatchTouch(frame, 'touchstart', [{ identifier: 1, clientX: 40, clientY: 140 }]);
    const move = dispatchTouch(frame, 'touchmove', [
      { identifier: 1, clientX: 42, clientY: 100 }
    ]);

    expect(state.get()).toBe(100);
    expect(move.defaultPrevented).toBe(true);
    cleanup();
  });

  it('leaves desktop links and normal click interaction untouched', () => {
    const scrollOwner = document.createElement('div');
    scrollOwner.style.overflowY = 'auto';
    mockScrollable(scrollOwner);
    const frame = frameInside(scrollOwner);
    const link = frame.contentDocument?.createElement('a') as HTMLAnchorElement;
    link.href = '#details';
    link.textContent = 'Open details';
    frame.contentDocument?.body.appendChild(link);
    const cleanup = installBodyFrameScrollBridge(frame);
    let clicks = 0;
    link.addEventListener('click', () => {
      clicks += 1;
    });

    const click = new MouseEvent('click', { bubbles: true, cancelable: true });
    link.dispatchEvent(click);

    expect(clicks).toBe(1);
    expect(click.defaultPrevented).toBe(false);
    cleanup();
  });

  it('leaves an active text selection gesture native', () => {
    const scrollOwner = document.createElement('div');
    scrollOwner.style.overflowY = 'auto';
    const state = mockScrollable(scrollOwner, { initial: 80 });
    const frame = frameInside(scrollOwner);
    const paragraph = frame.contentDocument?.createElement('p') as HTMLParagraphElement;
    paragraph.textContent = 'Selectable message text';
    frame.contentDocument?.body.appendChild(paragraph);

    const range = frame.contentDocument?.createRange() as Range;
    range.selectNodeContents(paragraph);
    const selection = frame.contentDocument?.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    const cleanup = installBodyFrameScrollBridge(frame);

    dispatchTouch(frame, 'touchstart', [{ identifier: 1, clientX: 20, clientY: 100 }]);
    const move = dispatchTouch(frame, 'touchmove', [
      { identifier: 1, clientX: 20, clientY: 60 }
    ]);

    expect(state.get()).toBe(80);
    expect(move.defaultPrevented).toBe(false);
    selection?.removeAllRanges();
    cleanup();
  });

  it('removes every bridge listener during cleanup', () => {
    const scrollOwner = document.createElement('div');
    scrollOwner.style.overflowY = 'auto';
    const state = mockScrollable(scrollOwner, { initial: 15 });
    const frame = frameInside(scrollOwner);
    const cleanup = installBodyFrameScrollBridge(frame);

    cleanup();
    const wheel = dispatchWheel(frame, { deltaY: 20 });
    dispatchTouch(frame, 'touchstart', [{ identifier: 1, clientX: 0, clientY: 40 }]);
    const touch = dispatchTouch(frame, 'touchmove', [
      { identifier: 1, clientX: 0, clientY: 20 }
    ]);

    expect(state.get()).toBe(15);
    expect(wheel.defaultPrevented).toBe(false);
    expect(touch.defaultPrevented).toBe(false);
  });
});
