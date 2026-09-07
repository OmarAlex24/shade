/**
 * Copy-to-clipboard for command blocks.
 *
 * Every `<button data-copy="…">` copies its `data-copy` payload and confirms
 * inline for two seconds. The confirmation is the whole point of the control,
 * so it stays on under reduced motion; only the fade is dropped by CSS.
 *
 * Clipboard access can fail (insecure origin, denied permission). The button
 * then reports the failure instead of pretending it worked.
 */

const RESET_MS = 2000;
const IDLE_LABEL = 'copy';

function label(button: HTMLButtonElement, text: string, copied: boolean): void {
  button.textContent = text;
  button.dataset.copied = String(copied);
}

async function copy(button: HTMLButtonElement): Promise<void> {
  const payload = button.dataset.copy;
  if (!payload) return;

  try {
    await navigator.clipboard.writeText(payload);
    label(button, 'copied', true);
  } catch {
    label(button, 'press ⌘C', false);
  }

  window.setTimeout(() => label(button, IDLE_LABEL, false), RESET_MS);
}

export function initCopy(): void {
  const buttons = document.querySelectorAll<HTMLButtonElement>('button[data-copy]');
  buttons.forEach((button) => {
    button.addEventListener('click', () => {
      void copy(button);
    });
  });
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initCopy);
  } else {
    initCopy();
  }
}
