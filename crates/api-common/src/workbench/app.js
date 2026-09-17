// Progressive enhancement only: navigation and responses remain server-owned.
(() => {
  'use strict';
  const root = document.documentElement;
  const theme = document.getElementById('theme');
  const media = matchMedia('(prefers-color-scheme: dark)');
  const applyTheme = () => {
    root.dataset.theme = theme.value === 'system' ? (media.matches ? 'dark' : 'light') : theme.value;
    root.dataset.themeChoice = theme.value;
    document.dispatchEvent(new CustomEvent('workbench-theme'));
  };
  theme.value = root.dataset.themeChoice || 'system';
  theme.addEventListener('change', () => {
    applyTheme();
    try { localStorage.setItem('meteocore-theme', theme.value); } catch (_) {}
  });
  media.addEventListener('change', () => { if (theme.value === 'system') applyTheme(); });

  document.querySelectorAll('[data-expiry]').forEach(label => {
    const expires = Date.parse(label.dataset.expiry);
    if (Number.isFinite(expires) && expires < Date.now()) {
      label.textContent = 'Expired · ' + label.dataset.expiry;
      label.classList.add('warning');
    }
  });

  let toastTimer;
  async function copy(text) {
    const toast = document.getElementById('toast');
    try {
      await navigator.clipboard.writeText(text);
      toast.textContent = 'Copied to clipboard';
    } catch (_) {
      // Clipboard access is unavailable on some HTTP origins. Keep the exact
      // request selectable rather than falsely reporting a successful copy.
      toast.textContent = 'Clipboard unavailable. Select and copy this text: ' + text;
    }
    clearTimeout(toastTimer);
    toast.classList.add('visible');
    toastTimer = setTimeout(() => toast.classList.remove('visible'), 7000);
  }
  document.querySelectorAll('[data-copy]').forEach(button => button.addEventListener('click', () => copy(button.dataset.copy)));

  document.querySelectorAll('.query-form').forEach(form => {
    const fields = form.querySelectorAll('[data-param]');
    const update = () => {
      fields.forEach(input => {
        // Preserve intentional empty exact-match predicates already present in
        // the response URL; otherwise omit unused optional API controls.
        if (input.value !== '' || input.dataset.keepEmpty === 'true') input.name = input.dataset.param;
        else input.removeAttribute('name');
      });
      const url = new URL(form.action, location.href);
      url.search = new URLSearchParams(new FormData(form));
      url.searchParams.set('f', 'json');
      const draft = form.querySelector('[data-draft]');
      if (draft) draft.textContent = url.href;
    };
    form.addEventListener('input', event => {
      if (event.target.name !== 'offset' && event.target.dataset.param !== 'offset') {
        const offset = form.querySelector('[name="offset"]');
        if (offset) offset.value = '0';
      }
      update();
    });
    form.querySelectorAll('[data-clear-predicate]').forEach(button => button.addEventListener('click', () => {
      const input = button.closest('[data-predicate]').querySelector('[data-param]');
      input.value = '';
      input.dataset.keepEmpty = 'false';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.focus();
    }));
    form.addEventListener('submit', update);
    update();
  });
  document.querySelectorAll('[data-view]').forEach(button => button.addEventListener('click', () => {
    document.querySelector('.collection-list')?.classList.toggle('cards', button.dataset.view === 'cards');
    document.querySelectorAll('[data-view]').forEach(other => other.setAttribute('aria-pressed', String(other === button)));
  }));
  document.getElementById('property-search')?.addEventListener('input', event => {
    const term = event.target.value.toLowerCase();
    document.querySelectorAll('[data-property]').forEach(row => { row.hidden = !row.dataset.property.toLowerCase().includes(term); });
  });

  // Browser history works without JS; enhancement also restores the exact
  // filtered list when a user follows a resource link and then its back link.
  try {
    const scope = document.querySelector('[data-results-scope]')?.dataset.resultsScope;
    if (scope) sessionStorage.setItem('meteocore-results:' + scope, location.href);
    document.querySelectorAll('[data-back-scope]').forEach(link => {
      const previous = sessionStorage.getItem('meteocore-results:' + link.dataset.backScope);
      if (!previous) return;
      const target = new URL(previous, location.href);
      const allowed = new URL(link.href, location.href);
      if (target.origin === allowed.origin && target.pathname === allowed.pathname) link.href = target.href;
    });
  } catch (_) {}
})();
