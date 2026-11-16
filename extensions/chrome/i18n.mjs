let language = normalizeLanguage(globalThis.navigator?.language);
let catalogs = {};
export function normalizeLanguage(value) { return typeof value === 'string' && /^ru(?:-|$)/i.test(value) ? 'ru' : 'en'; }
export function setLanguage(value) { language = normalizeLanguage(value); return language; }
export async function loadTranslations() {
  for (const locale of ['en', 'ru']) {
    const response = await fetch(new URL(`_locales/${locale}/messages.json`, import.meta.url));
    if (!response.ok) throw new Error(`Could not load ${locale} translations`);
    catalogs[locale] = await response.json();
  }
}
export function translate(text, substitutions = []) {
  let result = String(text);
  // Also recognize already-rendered errors when the chat changes language.
  const entries = Object.keys(catalogs.ru || {}).map(key => [catalogs.ru[key].message, catalogs.en[key].message]);
  for (const [ru, en] of entries.sort((a,b) => b[1].length-a[1].length)) result = result.split(en).join(ru);
  if (language === 'en') {
    for (const [ru, en] of entries.sort((a,b) => b[0].length-a[0].length)) result = result.split(ru).join(en);
  }
  return result.replace(/\$(\d+)/g, (match, index) => substitutions[Number(index)-1] ?? match);
}
export function injectionMessages() {
  return Object.fromEntries(Object.keys(catalogs.ru || {}).map(key => [catalogs.ru[key].message, catalogs[language][key].message]));
}
export function localizeDocument(document) {
  document.documentElement.lang = language;
  for (const element of document.querySelectorAll('[data-i18n]')) element.textContent = translate(element.dataset.i18n);
  for (const attribute of ['aria-label', 'title']) {
    for (const element of document.querySelectorAll(`[data-i18n-${attribute}]`)) element.setAttribute(attribute, translate(element.getAttribute(`data-i18n-${attribute}`)));
  }
}
