// Apply the saved preference before first paint. Storage may be unavailable.
(() => {
  document.documentElement.classList.add('js');
  let choice = 'system';
  try { choice = localStorage.getItem('meteocore-theme') || 'system'; } catch (_) {}
  if (!['light', 'dark', 'system'].includes(choice)) choice = 'system';
  document.documentElement.dataset.themeChoice = choice;
  document.documentElement.dataset.theme = choice === 'system'
    ? (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light') : choice;
})();
