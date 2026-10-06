'use strict';

(() => {
  const key = 'tucanotime-colour-mode';
  const system = typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null;
  let choice = null;
  try {
    const stored = localStorage.getItem(key);
    if (stored === 'light' || stored === 'dark') choice = stored;
  } catch {}
  function apply(mode) {
    document.documentElement.dataset.theme = mode;
    document.documentElement.classList.toggle('is-dark', mode === 'dark');
    document.documentElement.classList.toggle('is-light', mode === 'light');
    document.body?.classList.toggle('is-dark', mode === 'dark');
    document.body?.classList.toggle('is-light', mode === 'light');
    document.documentElement.style.colorScheme = mode;
    document.querySelectorAll('input[name="colour-mode"]').forEach((input) => {
      input.checked = input.value === mode;
    });
  }
  apply(choice || (system?.matches ? 'dark' : 'light'));
  system?.addEventListener('change', (event) => {
    if (!choice) apply(event.matches ? 'dark' : 'light');
  });
  document.addEventListener('DOMContentLoaded', () => {
    apply(document.documentElement.dataset.theme);
    document.querySelectorAll('input[name="colour-mode"]').forEach((input) => {
      input.addEventListener('change', () => {
        if (!input.checked) return;
        choice = input.value;
        try { localStorage.setItem(key, choice); } catch {}
        apply(choice);
      });
    });
  });
})();