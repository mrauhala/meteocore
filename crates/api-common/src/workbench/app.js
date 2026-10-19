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
  document.querySelectorAll('[data-sort]').forEach(select => select.addEventListener('change', event => {
    const url = new URL(location.href); url.searchParams.set('sortby', event.target.value); url.searchParams.delete('offset'); url.searchParams.set('f','html'); location.href = url.href;
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
    // A field the request was rejected for stays in view (#1132), as does
    // the server's choice to open it: an items request that filters or
    // sorts (#1143). An Overview chip names the builder in the fragment.
    const invalid = details.querySelector('[aria-invalid="true"]') !== null;
    const requested = details.id === 'item-builder' && /^#(item-builder|filter=|sort=)/.test(location.hash);
    let stored = false;
    try { stored = sessionStorage.getItem(key) === 'open'; } catch (_) {}
    details.open = details.open || invalid || requested || stored;
    // Only the reader's own choice is remembered: a toggle event also
    // follows the opening above, which must not outlast this page.
    const remember = open => {
      try { sessionStorage.setItem(key,open ? 'open' : 'closed'); } catch (_) {}
    };
    details.querySelector(':scope > summary')?.addEventListener('click',() => remember(!details.open));
    form.addEventListener('submit',() => remember(details.open));
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
    // The pickers fill `datetime` (#1143): UTC, `..` for an open end. They
    // listen before the form, so the draft sees the joined value.
    const datetime = form.querySelector('[data-param="datetime"]');
    const pickers = [form.querySelector('[data-datetime-start]'), form.querySelector('[data-datetime-end]')];
    if (datetime && pickers.every(Boolean)) {
      const bound = input => input.value ? (input.value.length === 16 ? input.value + ':00' : input.value) + 'Z' : '..';
      pickers.forEach(picker => picker.addEventListener('input', () => {
        datetime.value = pickers.some(p => p.value) ? pickers.map(bound).join('/') : '';
      }));
    }
    // A sortable property and a direction make `sortby`; a value the
    // selects cannot show (several keys) stays as given.
    const sortProperty = form.querySelector('[data-sort-property]');
    const sortDirection = form.querySelector('[data-sort-direction]');
    const sortby = form.querySelector('[data-sortby]');
    if (sortProperty && sortDirection && sortby) {
      const compose = () => {
        const raw = sortProperty.selectedOptions[0]?.hasAttribute('data-raw');
        sortby.value = !sortProperty.value ? '' : raw ? sortProperty.value : sortDirection.value + sortProperty.value;
        sortDirection.disabled = !sortProperty.value || raw;
      };
      sortProperty.addEventListener('input', compose); sortDirection.addEventListener('input', compose);
      compose();
    }
    // The map's view as the box, from the items page's map (map.js keeps
    // it in `data-view`).
    const useView = form.querySelector('[data-use-map-view]');
    const mapElement = document.getElementById('feature-map');
    if (useView && mapElement) {
      useView.hidden = false;
      useView.addEventListener('click', () => {
        const bbox = form.querySelector('[data-param="bbox"]');
        const status = form.querySelector('[data-map-view-status]');
        if (mapElement.hidden || !mapElement.dataset.view) { status.textContent = 'The map is not available. Enter the bounds.'; return; }
        bbox.value = mapElement.dataset.view; status.textContent = '';
        bbox.dispatchEvent(new Event('input', { bubbles: true }));
      });
    }
    // An Overview chip opens the builder at its property (#1143).
    const chosen = /^#(filter|sort)=(.+)$/.exec(location.hash);
    if (chosen) {
      let name = chosen[2];
      try { name = decodeURIComponent(name); } catch (_) {}
      const select = form.querySelector(chosen[1] === 'filter' ? '[data-new-property]' : '[data-sort-property]');
      if (select && [...select.options].some(option => option.value === name)) {
        select.value = name;
        select.dispatchEvent(new Event('input', { bubbles: true }));
        (chosen[1] === 'filter' ? form.querySelector('[data-new-value]') : sortDirection)?.focus();
      }
    }
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
    // Kept in this browser for the collection, the same on every API root
    // serving it (#1143).
    const key = 'meteocore-item-columns:' + itemProperties.dataset.collection;
    const status = document.querySelector('[data-column-status]');
    function cellValue(cell, value, present) {
      if (!present) { cell.textContent = 'Absent'; return; }
      if (value === null) { cell.textContent = 'null'; return; }
      if (Array.isArray(value) && value.every(v => v === null || typeof v !== 'object')) {
        if (!value.length) { cell.textContent = 'Empty array'; return; }
        const chips=document.createElement('span'); chips.className='chip-row';
        value.forEach(v => { const chip=document.createElement('span'); chip.className='chip'; cellValue(chip,v,true); chips.append(chip); });
        cell.append(chips); return;
      }
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
      const saved = JSON.parse(localStorage.getItem(key));
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
      applyColumns(names); try { localStorage.setItem(key,JSON.stringify(names)); } catch (_) {}
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
