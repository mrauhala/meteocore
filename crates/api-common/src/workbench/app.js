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

  document.getElementById('api-select')?.addEventListener('change', event => { location.href = event.target.value; });
  document.getElementById('help-button')?.addEventListener('click', () => document.getElementById('help-dialog').showModal());
  document.getElementById('close-help')?.addEventListener('click', () => document.getElementById('help-dialog').close());
  document.querySelectorAll('[data-page-size]').forEach(select => select.addEventListener('change', event => {
    const url = new URL(location.href); url.searchParams.set('limit', event.target.value); url.searchParams.delete('offset'); url.searchParams.set('f','html'); location.href = url.href;
  }));
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
  document.querySelectorAll('[data-disclosure]').forEach(details => {
    const form = details.closest('form') || details.querySelector('form');
    if (!form) return;
    const prefix = details.dataset.disclosure === 'collection-search' ? 'meteocore-search-disclosure:' : 'meteocore-disclosure:' + details.dataset.disclosure + ':';
    const key = prefix + new URL(form.action,location.href).pathname;
    try { details.open = sessionStorage.getItem(key) === 'open'; } catch (_) {}
    const remember = () => {
      try { sessionStorage.setItem(key,details.open ? 'open' : 'closed'); } catch (_) {}
    };
    details.addEventListener('toggle',remember);
    form.addEventListener('submit',remember);
  });

  document.querySelectorAll('.query-form').forEach(form => {
    const fields = form.querySelectorAll('[data-param]');
    let edited = false;
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
      if (!edited) {
        const preview = form.querySelector('[data-draft-disclosure]');
        if (preview) preview.open = true;
        edited = true;
      }
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
  const collectionList = document.querySelector('.collection-list');
  const viewButtons = document.querySelectorAll('[data-view]');
  if (collectionList && viewButtons.length) {
    const viewKey = 'meteocore-collection-view:' + location.pathname;
    const applyView = view => {
      collectionList.classList.toggle('collection-grid',view === 'cards');
      viewButtons.forEach(button => {
        button.setAttribute('aria-pressed',String(button.dataset.view === view));
        button.classList.toggle('selected',button.dataset.view === view);
      });
    };
    try { applyView(sessionStorage.getItem(viewKey) === 'cards' ? 'cards' : 'list'); } catch (_) {}
    viewButtons.forEach(button => button.addEventListener('click',() => {
      applyView(button.dataset.view);
      try { sessionStorage.setItem(viewKey,button.dataset.view); } catch (_) {}
    }));
  }
  // Column choices affect this HTML view only, never the API request.
  const itemProperties = document.getElementById('item-properties');
  if (itemProperties) {
    const rows = JSON.parse(itemProperties.textContent);
    const table = document.querySelector('.item-table');
    const choices = [...document.querySelectorAll('[data-item-column]')];
    const key = 'meteocore-item-columns:' + location.pathname;
    const status = document.querySelector('[data-column-status]');
    function cellValue(cell, value, present) {
      if (!present) { cell.textContent = 'Absent'; return; }
      if (value === null) { cell.textContent = 'null'; return; }
      if (typeof value === 'object') {
        const details = document.createElement('details'), summary = document.createElement('summary'), pre = document.createElement('pre');
        details.className = 'property-value';
        summary.textContent = Array.isArray(value) ? `array · ${value.length} values` : `object · ${Object.keys(value).length} keys`;
        pre.textContent = JSON.stringify(value,null,2); details.append(summary,pre); cell.append(details); return;
      }
      if (typeof value === 'string' && /^https?:\/\//.test(value)) {
        const link = document.createElement('a'); link.href = value; link.textContent = value; cell.append(link); return;
      }
      cell.textContent = value === '' ? 'Empty string' : String(value);
    }
    function applyColumns(names) {
      choices.forEach(c => { c.checked = names.includes(c.value); c.disabled = names.length >= 8 && !c.checked; });
      table.querySelectorAll('[data-property-cell]').forEach(cell => cell.remove());
      const header = table.querySelector('thead tr');
      names.forEach(name => { const th = document.createElement('th'); th.scope = 'col'; th.dataset.propertyCell = ''; th.textContent = name; header.append(th); });
      table.querySelectorAll('tbody tr').forEach((row,index) => names.forEach(name => {
        const td = document.createElement('td'); td.dataset.propertyCell = '';
        cellValue(td,rows[index]?.[name],Object.hasOwn(rows[index] || {},name)); row.append(td);
      }));
      status.textContent = `${names.length} property columns selected`;
    }
    try {
      const saved = JSON.parse(sessionStorage.getItem(key));
      if (Array.isArray(saved) && saved.every(n => typeof n === 'string')) {
        // Retain selected fields even when absent from a later response page.
        saved.slice(0,8).forEach(name => {
          if (choices.some(c => c.value === name)) return;
          const label=document.createElement('label'), input=document.createElement('input');
          input.type='checkbox'; input.value=name; input.dataset.itemColumn='';
          label.append(input,document.createTextNode(name)); document.querySelector('.column-options').append(label); choices.push(input);
        });
        applyColumns(saved.slice(0,8));
      }
    } catch (_) { /* Server-rendered defaults remain usable. */ }
    choices.forEach(choice => choice.addEventListener('change',() => {
      const names=choices.filter(c => c.checked).map(c => c.value).slice(0,8);
      applyColumns(names); try { sessionStorage.setItem(key,JSON.stringify(names)); } catch (_) {}
    }));
  }
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
