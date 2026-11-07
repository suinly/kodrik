import {computed, ref} from 'vue'
import {catalogs, type TranslationKey} from './translations.ts'

const deviceTranslations = {
  'Непрочитанных: {0}': 'Unread: {0}',
  'Подключения устройств': 'Device connections',
  'UUID запроса подключения': 'Pairing request UUID',
  'Код сравнения': 'Comparison code',
  'Сверьте UUID и код с терминалом на вашем устройстве. Подтверждайте только запрос, который вы начали сами.': 'Compare the UUID and code with the terminal on your device. Only approve a request you initiated yourself.',
  'Я сверил UUID и код на своём устройстве': 'I compared the UUID and code on my device',
  'Подтвердить подключение': 'Approve connection',
  'Подключение подтверждено.': 'Connection approved.',
  'Отозвать доступ': 'Revoke access',
  'Нет зарегистрированных устройств.': 'No registered devices.',
  'Загружаю устройства…': 'Loading devices…',
  'Доступ до {0}': 'Access expires {0}',
  'Закрытие этой страницы не отзывает доступ устройства. Доступ к среде заканчивается при закрытии TUI.': 'Closing this page does not revoke device access. Environment access ends when the TUI closes.',
  'Среда выполнения': 'Execution environment',
  'Подключено': 'Connected',
  'Отключено': 'Disconnected',
  'Файл остался на отключённом устройстве.': 'The file remains on the disconnected device.',
  'Локальный файл не сохранён на сервере.': 'The local file is not saved on the server.',
  'Я просмотрел полный вызов инструмента': 'I reviewed the full invocation',
}
Object.assign(catalogs.en, deviceTranslations)
Object.assign(catalogs.ru, Object.fromEntries(Object.keys(deviceTranslations).map(key=>[key,key])))

export type Language = 'en' | 'ru'
export type LanguagePreference = Language | 'system'
export const languageKey = 'kodrik.language'
export function detectLanguage(value: string | undefined): Language {
  return /^ru(?:-|$)/i.test(value ?? '') ? 'ru' : 'en'
}
export function initialLanguage(browserLanguage?: string, saved?: string | null): Language {
  return saved === 'en' || saved === 'ru' ? saved : detectLanguage(browserLanguage)
}
function savedLanguage(): LanguagePreference {
  if (typeof window === 'undefined') return 'system'
  let saved: string | null = null
  try { saved = globalThis.localStorage?.getItem(languageKey) ?? null } catch { /* Private browsers can disable storage. */ }
  return saved === 'en' || saved === 'ru' ? saved : 'system'
}
const browserLocale = ref(globalThis.navigator?.language)
if (typeof window !== 'undefined') window.addEventListener('languagechange', () => { browserLocale.value = navigator.language })
export const languagePreference = ref<LanguagePreference>(savedLanguage())
export const language = computed(() => initialLanguage(browserLocale.value, languagePreference.value))
export const locale = computed(() => language.value === 'ru' ? 'ru-RU' : 'en-US')
export function setLanguage(value: LanguagePreference) {
  languagePreference.value = value
  try { globalThis.localStorage?.setItem(languageKey, value) } catch { /* The choice still applies to this tab. */ }
}
export function formatUiValue(value: unknown): string {
  if (value === null || value === undefined) return t('Нет данных')
  return typeof value === 'number' ? new Intl.NumberFormat(locale.value, {maximumFractionDigits: 6}).format(value) : String(value)
}

const englishKeys = new Map<string, string>(Object.entries(catalogs.en).map(([key, value]) => [value, key]))
const escapePattern = (value: string) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
const templates = Object.entries(catalogs.en).flatMap(([key, value]) => {
  if (!key.includes('{0}')) return []
  return [key, value].map(text => ({key, expression: new RegExp(`^${text.split(/\{\d+\}/).map(escapePattern).join('(.*?)')}$`, 's')}))
})

// Existing status/error strings may have been created before a language switch.
export function t(key: string, ...values: unknown[]): string {
  const current = catalogs[language.value]
  if (Object.hasOwn(current, key)) return current[key as TranslationKey].replace(/\{(\d+)\}/g, (_, index) => String(values[Number(index)] ?? ''))
  const original = englishKeys.get(key)
  if (original) return current[original as TranslationKey]
  for (const template of templates) {
    const match = template.expression.exec(key)
    if (match) return current[template.key as TranslationKey].replace(/\{(\d+)\}/g, (_, index) => match[Number(index) + 1] ?? '')
  }
  return key
}
