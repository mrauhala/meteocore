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

  document.getElementById('api-select')?.addEventListener('change', event => { location.href = event.target.value; });
  document.getElementById('help-button')?.addEventListener('click', () => document.getElementById('help-dialog').showModal());
  document.getElementById('close-help')?.addEventListener('click', () => document.getElementById('help-dialog').close());
  document.querySelector('[data-page-size]')?.addEventListener('change', event => {
    const url = new URL(location.href); url.searchParams.set('limit', event.target.value); url.searchParams.delete('offset'); url.searchParams.set('f','html'); location.href = url.href;
  });
  function collectionTab() {
    const active = location.hash === '#metadata' ? 'metadata' : 'overview';
    document.querySelectorAll('[data-collection-view]').forEach(view => { view.hidden = view.id !== active; });
    document.querySelectorAll('[data-collection-tab]').forEach(link => {
      link.classList.toggle('active', link.dataset.collectionTab === active);
      if (link.dataset.collectionTab === active) link.setAttribute('aria-current','page'); else link.removeAttribute('aria-current');
    });
  }
  collectionTab(); window.addEventListener('hashchange',collectionTab);
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

  // Disclosure state is a UI preference, never an API query parameter.
  document.querySelectorAll('[data-disclosure="collection-search"]').forEach(details => {
    const form = details.closest('form');
    const key = 'meteocore-search-disclosure:' + new URL(form.action,location.href).pathname;
    try { details.open = sessionStorage.getItem(key) === 'open'; } catch (_) {}
    const remember = () => {
      try { sessionStorage.setItem(key,details.open ? 'open' : 'closed'); } catch (_) {}
    };
    details.addEventListener('toggle',remember);
    form.addEventListener('submit',remember);
  });

  document.querySelectorAll('.query-form').forEach(form => {
    const fields = form.querySelectorAll('[data-param]');
    const update = () => {
      const bounds = [...form.querySelectorAll('[data-bbox]')];
      if (bounds.length) {
        const values = bounds.map(input => input.value);
        const partial = values.some(Boolean) && !values.every(Boolean);
        bounds[0].setCustomValidity(partial ? 'Enter all four area bounds, or leave them all blank.' : '');
        form.querySelector('[data-param="bbox"]').value = values.some(Boolean) ? values.join(',') : '';
      }
      const property = form.querySelector('[data-new-property]');
      const newValue = form.querySelector('[data-new-value]');
      if (property && newValue) {
        newValue.dataset.param = property.value;
        property.setCustomValidity(newValue.value && !property.value ? 'Choose a property for this value.' : '');
      }
      fields.forEach(input => {
        // Preserve intentional empty exact-match predicates already present in
        // the response URL; otherwise omit unused optional API controls.
        if (input.dataset.param && (input.value !== '' || input.dataset.keepEmpty === 'true')) input.name = input.dataset.param;
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
    document.querySelector('.collection-list')?.classList.toggle('collection-grid', button.dataset.view === 'cards');
    document.querySelectorAll('[data-view]').forEach(other => { other.setAttribute('aria-pressed', String(other === button)); other.classList.toggle('selected', other === button); });
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
