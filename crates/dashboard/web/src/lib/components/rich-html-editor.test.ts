import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

import RichHtmlEditor from './RichHtmlEditor.svelte';

function installFrameDocument(frame: HTMLIFrameElement): Document {
  const srcdoc = frame.getAttribute('srcdoc') ?? '';
  const doc = frame.contentDocument;
  if (!doc) throw new Error('iframe has no contentDocument');
  doc.open();
  doc.write(srcdoc);
  doc.close();
  return doc;
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('RichHtmlEditor', () => {
  it('opens sanitized HTML directly editable in a scriptless sandbox', async () => {
    render(RichHtmlEditor, {
      html:
        '<p onclick="evil()">Hello<script>alert(1)</script></p>' +
        '<form><input value="secret"></form><iframe src="https://evil.example"></iframe>' +
        '<img src="https://tracker.example/pixel.gif" alt="remote">' +
        '<a href="javascript:evil()">bad</a><a href="https://example.com">safe link</a>'
    });

    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const sandbox = frame.getAttribute('sandbox') ?? '';
    expect(sandbox).toBe('allow-same-origin');
    expect(sandbox).not.toMatch(/allow-(scripts|popups|top-navigation|downloads|forms)/);

    const srcdoc = frame.getAttribute('srcdoc') ?? '';
    expect(srcdoc).toContain('Hello');
    expect(srcdoc).not.toMatch(/<script|<form|<input|<iframe|onclick|javascript:/i);
    expect(srcdoc).not.toMatch(/<a[^>]*\shref="https:\/\/example\.com"/);
    expect(srcdoc).toContain('data-env-href="https://example.com"');
    expect(srcdoc).toContain('data-env-src="https://tracker.example/pixel.gif"');
    expect(srcdoc).not.toMatch(/<img[^>]*\ssrc="https:\/\/tracker\.example\/pixel\.gif"/i);
    expect(srcdoc).toContain("default-src 'none'");
    expect(srcdoc).toContain("script-src 'none'");
    expect(srcdoc).toContain("connect-src 'none'");
    expect(srcdoc).toContain('img-src data: cid:;');
    expect(srcdoc).not.toContain('img-src data: cid: https:');
    expect(srcdoc).toContain("form-action 'none'");
    expect(srcdoc).toContain("base-uri 'none'");

    const doc = installFrameDocument(frame);
    await fireEvent.load(frame);
    const editor = doc.getElementById('env-editor');
    expect(editor).toHaveAttribute('contenteditable', 'true');
    expect(editor).toHaveAttribute('role', 'textbox');
    expect(editor).toHaveAttribute('aria-label', 'Message HTML body');
    expect(editor).toHaveAttribute('aria-multiline', 'true');
  });

  it('disables contenteditable and formatting controls when read-only', async () => {
    render(RichHtmlEditor, { html: '<p>Locked</p>', disabled: true });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    await fireEvent.load(frame);

    expect(doc.getElementById('env-editor')).toHaveAttribute('contenteditable', 'false');
    expect(screen.getByRole('button', { name: 'Bold' })).toBeDisabled();
  });

  it('serializes rich input as sanitized HTML', async () => {
    const onchange = vi.fn();
    render(RichHtmlEditor, { html: '<p>Original</p>', onchange });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    await fireEvent.load(frame);

    const editor = doc.getElementById('env-editor') as HTMLElement;
    editor.innerHTML =
      '<p><strong>Rewritten</strong><img src="x" onerror="evil()"></p>' +
      '<script>alert(1)</script>';
    await fireEvent.input(editor);

    expect(onchange).toHaveBeenLastCalledWith('<p><strong>Rewritten</strong><img></p>');
  });

  it('sanitizes pasted HTML before insertion and blocks link navigation', async () => {
    const onchange = vi.fn();
    render(RichHtmlEditor, { html: '<p>Start</p>', onchange });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    await fireEvent.load(frame);

    const editor = doc.getElementById('env-editor') as HTMLElement;
    const range = doc.createRange();
    range.selectNodeContents(editor);
    range.collapse(false);
    doc.getSelection()?.removeAllRanges();
    doc.getSelection()?.addRange(range);

    const paste = new Event('paste', { bubbles: true, cancelable: true });
    Object.defineProperty(paste, 'clipboardData', {
      value: {
        getData: (type: string) =>
          type === 'text/html'
            ? '<a href="javascript:evil()" onclick="evil()">pasted</a><script>evil()</script>'
            : 'pasted'
      }
    });
    editor.dispatchEvent(paste);

    expect(paste.defaultPrevented).toBe(true);
    expect(onchange).toHaveBeenLastCalledWith('<p>Start</p><a>pasted</a>');
    const link = editor.querySelector('a') as HTMLAnchorElement;
    const click = new MouseEvent('click', { bubbles: true, cancelable: true });
    link.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
  });

  it('preserves line breaks in plain-text paste', async () => {
    const onchange = vi.fn();
    render(RichHtmlEditor, { html: '<p>Start</p>', onchange });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    await fireEvent.load(frame);

    const editor = doc.getElementById('env-editor') as HTMLElement;
    const range = doc.createRange();
    range.selectNodeContents(editor);
    range.collapse(false);
    doc.getSelection()?.removeAllRanges();
    doc.getSelection()?.addRange(range);

    const paste = new Event('paste', { bubbles: true, cancelable: true });
    Object.defineProperty(paste, 'clipboardData', {
      value: {
        getData: (type: string) => (type === 'text/plain' ? 'line one\nline two' : '')
      }
    });
    editor.dispatchEvent(paste);

    expect(onchange).toHaveBeenLastCalledWith('<p>Start</p>line one<br>line two');
  });

  it('executes every compact formatting control against the iframe selection', async () => {
    render(RichHtmlEditor, { html: '<p>Format me</p>' });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    const execCommand = vi.fn((_command: string, _showUi?: boolean, _value?: string) => true);
    Object.defineProperty(doc, 'execCommand', { configurable: true, value: execCommand });
    vi.spyOn(window, 'prompt').mockReturnValue('https://example.com/details');
    await fireEvent.load(frame);

    for (const name of ['Bold', 'Italic', 'Underline', 'Insert link', 'Ordered list', 'Unordered list', 'Clear formatting']) {
      await fireEvent.click(screen.getByRole('button', { name }));
    }

    expect(execCommand.mock.calls.map(([command]) => command)).toEqual([
      'bold',
      'italic',
      'underline',
      'createLink',
      'insertOrderedList',
      'insertUnorderedList',
      'removeFormat',
      'unlink'
    ]);
    expect(execCommand).toHaveBeenCalledWith('createLink', false, 'https://example.com/details');
  });

  it('restores the last valid iframe selection for keyboard toolbar commands', async () => {
    render(RichHtmlEditor, { html: '<p>Format me</p>' });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    const editor = doc.getElementById('env-editor') as HTMLElement;
    await fireEvent.load(frame);

    const text = editor.querySelector('p')?.firstChild;
    if (!text) throw new Error('fixture has no editable text');
    const intended = doc.createRange();
    intended.setStart(text, 0);
    intended.setEnd(text, 6);
    const selection = doc.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(intended);
    doc.dispatchEvent(new Event('selectionchange'));

    const outside = doc.createRange();
    outside.selectNodeContents(doc.body);
    outside.collapse(false);
    selection?.removeAllRanges();
    selection?.addRange(outside);

    const selectedAtCommand: string[] = [];
    Object.defineProperty(doc, 'execCommand', {
      configurable: true,
      value: vi.fn(() => {
        selectedAtCommand.push(doc.getSelection()?.toString() ?? '');
        return true;
      })
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Bold' }));

    expect(selectedAtCommand).toEqual(['Format']);
  });

  it('commits a toolbar mutation even when the browser emits no input event', async () => {
    const onchange = vi.fn();
    render(RichHtmlEditor, { html: '<p>Format me</p>', onchange });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    const editor = doc.getElementById('env-editor') as HTMLElement;
    Object.defineProperty(doc, 'execCommand', {
      configurable: true,
      value: vi.fn(() => {
        editor.innerHTML = '<p><strong>Format me</strong></p>';
        return true;
      })
    });
    await fireEvent.load(frame);

    await fireEvent.click(screen.getByRole('button', { name: 'Bold' }));

    expect(onchange).toHaveBeenLastCalledWith('<p><strong>Format me</strong></p>');
  });

  it('keeps a toolbar-created link inert in the frame but serializes its href', async () => {
    const onchange = vi.fn();
    render(RichHtmlEditor, { html: '<p>Link me</p>', onchange });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    const editor = doc.getElementById('env-editor') as HTMLElement;
    Object.defineProperty(doc, 'execCommand', {
      configurable: true,
      value: vi.fn(() => {
        editor.innerHTML = '<p><a href="https://example.com">Link me</a></p>';
        return true;
      })
    });
    vi.spyOn(window, 'prompt').mockReturnValue('https://example.com');
    await fireEvent.load(frame);

    await fireEvent.click(screen.getByRole('button', { name: 'Insert link' }));

    const link = editor.querySelector('a') as HTMLAnchorElement;
    expect(link).not.toHaveAttribute('href');
    expect(link).toHaveAttribute('data-env-href', 'https://example.com');
    expect(onchange).toHaveBeenLastCalledWith(
      '<p><a href="https://example.com">Link me</a></p>'
    );
  });

  it('refuses dangerous link URLs', async () => {
    render(RichHtmlEditor, { html: '<p>Format me</p>' });
    const frame = screen.getByTitle('Rich text message editor') as HTMLIFrameElement;
    const doc = installFrameDocument(frame);
    const execCommand = vi.fn((_command: string, _showUi?: boolean, _value?: string) => true);
    Object.defineProperty(doc, 'execCommand', { configurable: true, value: execCommand });
    vi.spyOn(window, 'prompt').mockReturnValue('javascript:alert(1)');
    await fireEvent.load(frame);

    await fireEvent.click(screen.getByRole('button', { name: 'Insert link' }));

    expect(execCommand).not.toHaveBeenCalled();
  });
});
