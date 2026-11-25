import {computed, ref} from 'vue'
import {catalogs, type TranslationKey} from './translations.ts'

const deviceTranslations = {
  "Число последовательных аварийных сбоев одного этапа задачи до приостановки автоматического восстановления. Переход к следующему этапу сбрасывает счётчик. 0 отключает автовосстановление. Не управляет простоем, темой диалога или перезапуском шлюза.": "Consecutive crashes within one task phase before automatic recovery is parked. Progress to the next phase resets the counter. 0 disables automatic recovery. Does not control inactivity, conversation topics or gateway restarts.",
  "За столько токенов до порога начинается подготовка сводки в фоне. При включённом сжатии значение меньше порога. 0 отключает только фоновую подготовку, не сжатие при достижении порога.": "Start preparing a summary this many tokens before the threshold. Must be below the threshold when compaction is enabled. 0 disables only background preparation, not compaction at the threshold.",
  "При такой оценке входного контекста старые сообщения в контексте модели заменяются сводкой; исходная история сохраняется. Это не лимит модели. 0 отключает автоматическое сжатие; иначе минимум 4096 токенов.": "At this estimated input-context size, older messages in the model context are replaced with a summary; original history is preserved. Not the model context limit. 0 disables automatic compaction; otherwise at least 4096 tokens.",

  "Значение HTTP-заголовка. Передаётся серверу как есть, включая префикс Bearer, если он нужен.": "HTTP header value. Sent as entered, including the Bearer prefix if required.",
  "Порог последовательных сбоев выполнения": "Consecutive execution crash threshold",
  "Запоминать выводы из бесед": "Save lessons from conversations",
  "Ежедневное исследование интересов": "Daily interest research",
  "Не раньше этого часа, 0–23": "Earliest daily research hour, 0–23",
  "Начало тихих часов, 0–23": "Quiet hours start, 0–23",
  "Конец тихих часов, 0–23": "Quiet hours end, 0–23",
  "Имя Telegram-шлюза": "Telegram gateway name",
  "Показывать ответ по мере генерации": "Preview replies during generation",
  "Порог сжатия истории, токены": "History compaction threshold, tokens",
  "Тип API / транспорта": "API / transport type",
  "Имя настроенного шлюза из раздела Telegram, а не имя бота или пользователя.": "Name of a configured Telegram gateway, not a bot or user name.",
  "Положительный числовой ID владельца в Telegram. Это не username и не ID группы.": "Positive numeric Telegram ID of the owner, not a username or group ID.",
  "Заменяет базовую системную инструкцию агента владельца. Не применяется к изолированным чатам. Для новых снимков контекста нужны перезапуск и новая сессия.": "Replaces the owner agent’s base system prompt. Does not apply to isolated chats. Restart and start a new session to use a new context snapshot.",
  "Запускает HTTP-сервер страниц и веб-чата в режиме шлюза.": "Starts the HTTP server for pages and web chat in gateway mode.",
  "Внешний URL сервера, включая префикс пути. Обязателен при включённом сервере. HTTPS; HTTP разрешён только для localhost и loopback.": "Public server URL, including any path prefix. Required when enabled. Use HTTPS; HTTP is only allowed for localhost and loopback.",
  "Числовой IP-адрес и порт HTTP-сервера, например 127.0.0.1:8080 или 0.0.0.0:8080. Это не внешний URL.": "Numeric HTTP bind IP and port, such as 127.0.0.1:8080 or 0.0.0.0:8080. Not the public URL.",
  "Точные IP-адреса прокси, от которых принимается X-Forwarded-For для определения IP клиента. Пустой список не доверяет этому заголовку.": "Exact proxy IPs allowed to supply the client IP via X-Forwarded-For. An empty list does not trust that header.",
  "Лимит одновременных SSE-подписок на обновления чатов одного пользователя во всех вкладках и расширении: 1–64. Не ограничивает число выполняемых задач. Общий лимит сервера — 64.": "Concurrent SSE chat update subscriptions per user across tabs and the extension: 1–64. Does not limit running tasks. The server-wide limit is 64.",
  "После содержательных успешных бесед извлекает устойчивые выводы для памяти компаньона. Не обучает веса модели.": "Extracts durable lessons from successful substantive conversations for companion memory. Does not train model weights.",
  "Разрешает не более одной попытки исследования интересов в день при работающем шлюзе, наличии активного интереса и не менее 10 минут без активности. Не отключает отдельно запланированные пробуждения.": "Allows at most one daily interest-research attempt while the gateway is running, with an active interest and at least 10 minutes of inactivity. Does not disable separately scheduled wakeups.",
  "Часовой пояс IANA для текущего времени агента, ежедневного исследования и тихих часов, например Europe/Moscow.": "IANA time zone for the agent’s current clock, daily research and quiet hours, for example Europe/Moscow.",
  "Самый ранний местный час ежедневной попытки исследования, не точное время отправки. Дополнительно учитываются тихие часы и активность.": "Earliest local hour for a daily research attempt, not an exact send time. Quiet hours and activity conditions also apply.",
  "Местный час начала запрета доставки инициативных сообщений и результатов пробуждений. Обычные ответы не блокируются. Интервал может переходить через полночь.": "Local start hour for withholding proactive messages and wakeup results. Ordinary replies are not blocked. The interval can span midnight.",
  "Местный час окончания тихого интервала: начиная с него доставка разрешена. Должен отличаться от часа начала.": "Local end hour of the quiet interval: delivery is allowed from this hour. Must differ from the start hour.",
  "Имя Telegram-шлюза для инициативных сообщений. Заполните вместе с ID личного чата. Если оба поля пусты, используется личный чат владельца.": "Telegram gateway name for proactive messages. Set together with the private chat ID. If both are empty, the owner’s private chat is used.",
  "Положительный ID личного Telegram-чата получателя. Заполните вместе с именем шлюза; бот должен иметь возможность писать в этот чат.": "Positive recipient private Telegram chat ID. Set together with the gateway name; the bot must be able to message this chat.",
  "Количество последовательных аварийных сбоев на одном этапе задачи, при достижении которого автоматическое восстановление приостанавливается. 0 отключает автовосстановление. Это не минуты и не проверка темы диалога.": "Consecutive crashes within one task phase that cause automatic recovery to be parked. 0 disables automatic recovery. Not a duration or a conversation-topic check.",
  "При такой оценке входного контекста старая история заменяется сводкой. Это не лимит модели. 0 отключает автоматическое сжатие; иначе минимум 4096 токенов.": "At this estimated input-context size, older history is replaced with a summary. Not the model context limit. 0 disables automatic compaction; otherwise at least 4096 tokens.",
  "За столько токенов до порога начинается подготовка сводки в фоне. Значение меньше порога. 0 отключает только фоновую подготовку, не сжатие при достижении порога.": "Start preparing a summary this many tokens before the threshold. Must be below the threshold. 0 disables only background preparation, not compaction at the threshold.",
  "Базовый HTTP(S) URL SearXNG, без /search: этот путь добавляется автоматически. Не задавайте одновременно с DeGoog.": "SearXNG HTTP(S) base URL without /search; the path is appended automatically. Do not set together with DeGoog.",
  "Базовый HTTP(S) URL DeGoog, без /api/search: этот путь добавляется автоматически. Не задавайте одновременно с SearXNG.": "DeGoog HTTP(S) base URL without /api/search; the path is appended automatically. Do not set together with SearXNG.",
  "Путь к исполняемому файлу Obscura или имя в PATH процесса шлюза. Это не команда с аргументами.": "Obscura executable path or a name resolved in the gateway process PATH. Not a shell command with arguments.",
  "openai — протокол OpenAI Responses API, в том числе совместимый сервер. Тип доступен только для чтения.": "openai selects the OpenAI Responses API protocol, including compatible servers. This type is read-only.",
  "Ключ доступа к выбранному API. Требуется непустое значение.": "Access key for the selected API. A nonempty value is required.",
  "Точный идентификатор модели, принимаемый этим API, а не произвольное название подключения.": "Exact model identifier accepted by this API, not a custom connection label.",
  "Базовый URL OpenAI-совместимого Responses API. Пустое значение использует стандартный адрес клиента OpenAI.": "Base URL for an OpenAI-compatible Responses API. An empty value uses the OpenAI client’s default address.",
  "telegram — подключение бота через Telegram Bot API. Тип доступен только для чтения.": "telegram connects a bot through the Telegram Bot API. This type is read-only.",
  "Токен Telegram-бота, выданный BotFather. Не пароль аккаунта Telegram.": "Telegram bot token issued by BotFather, not a Telegram account password.",
  "Публикует и обновляет предварительный текст ответа во время генерации. При выключении готовый ответ отправляется после завершения.": "Posts and updates a reply preview during generation. When disabled, the completed reply is sent after generation finishes.",
  "http — транспорт Streamable HTTP для инструментов MCP. stdio и старый HTTP+SSE не поддерживаются.": "http selects Streamable HTTP for MCP tools. stdio and legacy HTTP+SSE are not supported.",
  "Полный HTTP(S) URL точки подключения MCP, например https://example.com/mcp.": "Full HTTP(S) MCP endpoint URL, for example https://example.com/mcp.",
  "HTTP-заголовки запросов к этому MCP-серверу, например Authorization. Значения могут содержать секреты.": "HTTP request headers for this MCP server, such as Authorization. Values may contain secrets.",
  "Задайте один поисковый сервис: SearXNG или DeGoog. Без него инструменты веб-поиска и чтения страниц не подключаются.": "Configure one search service: SearXNG or DeGoog. Without it, web search and page-reading tools are not registered.",
  "Необязательное поле; пустое значение использует поведение по умолчанию.": "Optional; leave empty to use the default behavior.",

  'Адрес, по которому вы открываете Kodrik в браузере, включая префикс пути. Например: https://example.com/kodrik/.': 'The address you use to open Kodrik, including any path prefix. For example: https://example.com/kodrik/.',
  'Локальный адрес и порт веб-сервера. Меняйте только при настройке сети.': 'Local web server address and port. Change only when configuring the network.',
  'IP-адреса ваших обратных прокси. Доверяйте только серверам, которые вы контролируете.': 'IP addresses of your reverse proxies. Only trust servers you control.',
  'Часовой пояс для инициативных сообщений и тихих часов. Например: Europe/Moscow.': 'Time zone for proactive messages and quiet hours. For example: Europe/Moscow.',
  'Час инициативных сообщений в выбранном часовом поясе: от 0 до 23.': 'Hour for proactive messages in the selected time zone: 0 to 23.',
  'С этого часа инициативные сообщения не доставляются. Тихие часы могут переходить через полночь.': 'Proactive messages are not delivered from this hour. Quiet hours can span midnight.',
  'С этого часа доставка снова разрешена. Начало и конец тихих часов должны различаться.': 'Delivery is allowed again from this hour. Quiet hours must start and end at different hours.',
  'Размер контекста, при котором история сжимается. 0 отключает сжатие; иначе укажите не меньше 4096 токенов.': 'Context size that triggers history compaction. Set 0 to disable; otherwise use at least 4096 tokens.',
  'За сколько токенов до порога начинать фоновое сжатие. Меньше порога; 0 отключает фоновое сжатие.': 'Tokens before the threshold to start background compaction. Must be below the threshold; 0 disables background compaction.',
  'Путь к программе Obscura для чтения сайтов. Если она доступна в PATH, оставьте obscura.': 'Path to Obscura for reading websites. Keep obscura if it is available in PATH.',
  'Для веб-поиска укажите адрес SearXNG или DeGoog, но не оба сразу. Оставьте оба пустыми, чтобы отключить поиск.': 'For web search, configure SearXNG or DeGoog, not both. Leave both empty to disable search.',
  'Меняйте только для совместимого API или своего прокси. Пустое поле использует стандартный адрес провайдера.': 'Only change for a compatible API or your own proxy. Leave empty to use the default provider address.',

  'Модели': 'Models',
  'Профили моделей и подключения к провайдерам ИИ.': 'Model profiles and AI provider connections.',
  'Боты Telegram и владелец шлюза.': 'Telegram bots and gateway owner.',
  'Поведение': 'Behavior',
  'Общение, инициативные сообщения и работа с контекстом.': 'Conversation, proactive messages and context management.',
  'Поиск': 'Search',
  'Поисковый сервис и чтение веб-страниц.': 'Search service and web page reading.',
  'Адрес сервера и подключения веб-чата.': 'Server address and live chat connections.',
  'Интеграции': 'Integrations',
  'Внешние инструменты через серверы MCP.': 'External tools connected through MCP servers.',
  'Разделы настроек': 'Settings sections',
  'Отбросить несохранённые изменения настроек?': 'Discard unsaved settings changes?',
  'Профили агентов': 'Agent profiles',
  'Настройте инструкции, модель, навыки и инструменты для лёгких агентов.': 'Set instructions, model, skills, and tools for lightweight agents.',
  'Открыть каталог профилей': 'Browse agent profiles',
  'Мои профили': 'My profiles',
  'Встроенные профили': 'Bundled profiles',
  'Черновики на проверку': 'Drafts for review',
  'Префикс builtin- зарезервирован для встроенных профилей.': 'The builtin- prefix is reserved for bundled profiles.',
  'ID: до 64 символов, строчные латинские буквы, цифры, дефисы и подчёркивания. Начните с буквы или цифры.': 'Use up to 64 lowercase letters, digits, hyphens, or underscores. Start with a letter or digit.',
  'Этот ID уже используется.': 'This ID is already in use.',
  'Навыки агента': 'Agent skills',
  'Инструменты агента': 'Agent tools',
  'Отбросить несохранённые изменения профиля?': 'Discard unsaved profile changes?',
  'Профиль сохранён.': 'Profile saved.',
  'Удалить профиль «{0}»?': 'Delete profile “{0}”?',
  'Профиль удалён.': 'Profile deleted.',
  'Загрузка профилей…': 'Loading profiles…',
  'Повторить загрузку': 'Retry',
  'Новый профиль агента': 'New agent profile',
  'Профилей пока нет.': 'No profiles yet.',
  'Этот профиль поставляется с Kodrik. Чтобы изменить его настройки, создайте копию.': 'This profile ships with Kodrik. To change its settings, make a copy.',
  'Проверьте инструкции и доступы черновика перед сохранением.': 'Review the draft’s instructions and permissions before saving.',
  'ID профиля': 'Profile ID',
  'Название профиля': 'Profile name',
  'Инструкции': 'Instructions',
  'Роль модели': 'Model role',
  'Модель по умолчанию': 'Default model',
  'Нет доступных вариантов.': 'No options available.',
  'Скопировать в новый профиль': 'Copy as new profile',
  'Сохранить профиль': 'Save profile',
  'Удалить профиль': 'Delete profile',
  'Удалить черновик': 'Delete draft',
  'Закрыть профиль': 'Close profile',
  'Токены: {0} · Ответы модели: {1}': 'Tokens: {0} · Model responses: {1}',
  'Входящие: {0} · Исходящие: {1} · Из кэша: {2}': 'Input: {0} · Output: {1} · Cached: {2}',
  'Выберите профиль или создайте новый.': 'Select a profile or create a new one.',
  'Нет несохранённых изменений.': 'No unsaved changes.',
  'Показывать ключи конфигурации': 'Show configuration keys',
  'Хранение и безопасность': 'Storage and security',
  'Дополнительные параметры': 'Advanced settings',

  'Владелец': 'Owner',
  'Системная инструкция': 'System prompt',
  'Веб-сервер': 'Web server',
  'Доверенные прокси': 'Trusted proxies',
  'Включено': 'Enabled',
  'Адрес прослушивания': 'Listen address',
  'Внешний адрес сервера': 'Public server URL',
  'Подключения на пользователя': 'Live connections per user',
  'Компаньон': 'Companion',
  'Обучение на основе бесед': 'Learn from conversations',
  'Инициативные сообщения': 'Proactive messages',
  'Интерфейс Telegram': 'Telegram interface',
  'ID пользователя Telegram': 'Telegram user ID',
  'ID личного чата Telegram': 'Private Telegram chat ID',
  'Часовой пояс': 'Time zone',
  'Час инициативных сообщений': 'Proactive message hour',
  'Начало тихих часов': 'Quiet hours start',
  'Конец тихих часов': 'Quiet hours end',
  'Сессии': 'Sessions',
  'Лимит перезапусков': 'Restart limit',
  'Порог сжатия контекста, токены': 'Context compaction threshold, tokens',
  'Запас для фонового сжатия, токены': 'Background compaction margin, tokens',
  'Поиск и чтение сайтов': 'Search and web reading',
  'Адрес SearXNG': 'SearXNG URL',
  'Адрес DeGoog': 'DeGoog URL',
  'Путь к Obscura': 'Obscura executable path',
  'Провайдеры ИИ': 'AI providers',
  'Провайдер': 'Provider',
  'Профиль модели': 'Model profile',
  'Идентификатор модели в API': 'API model ID',
  'Уровень рассуждений': 'Reasoning effort',
  'Цены за токены': 'Token prices',
  'Входные токены, USD за 1 млн': 'Input tokens, USD per million',
  'Кэшированные входные токены, USD за 1 млн': 'Cached input tokens, USD per million',
  'Выходные токены, USD за 1 млн': 'Output tokens, USD per million',
  'Именованные подключения к API. Один провайдер может использоваться в нескольких профилях. Прежние настройки с моделями в [provider], включая system_one и primary, по-прежнему работают.': 'Named API connections. Multiple profiles can use the same provider. Legacy setups with models under [provider], including system_one and primary, still work.',
  'Роли профилей: fast — быстрые и фоновые задачи, smart — обычные диалоги, deep — сложные задачи, vision — изображения. Если задан [model.smart], «Авто» начинает со smart; в прежней конфигурации «Авто» начинает с fast.': 'Profile roles: fast for quick and background tasks, smart for regular conversations, deep for complex tasks, and vision for images. Auto starts with smart when [model.smart] is configured; legacy configurations still start with fast.',
  'Имя существующего подключения из [provider], например openai для [provider.openai].': 'Name of an existing [provider] connection, for example openai for [provider.openai].',
  'Точный идентификатор модели для прежней конфигурации. Для нового подключения задайте его в профиле модели.': 'Exact model identifier for a legacy configuration. For new setups, set it in a model profile.',
  'Необязательный уровень рассуждений: none, minimal, low, medium, high, xhigh или max. Поддержка зависит от модели; пустое поле не отправляет параметр.': 'Optional reasoning effort: none, minimal, low, medium, high, xhigh, or max. Support varies by model; leave empty to omit the parameter.',
  'Необязательная цена в долларах США за миллион токенов для оценки стоимости.': 'Optional price in US dollars per million tokens for cost estimates.',
  'Тип подключения': 'Connection type',
  'Ключ API': 'API key',
  'Базовый адрес API': 'API base URL',
  'Модель': 'Model',
  'Шлюзы Telegram': 'Telegram gateways',
  'Токен бота': 'Bot token',
  'Потоковые ответы': 'Stream responses',
  'Серверы MCP': 'MCP servers',
  'Адрес сервера': 'Server URL',
  'HTTP-заголовки': 'HTTP headers',

  'Одновременные подключения веб-чата и расширения на пользователя: от 1 до 64. После сохранения перезапустите шлюз.': 'Concurrent web chat and extension connections per user: 1–64. Restart the gateway after saving.',
  'При сохранении TOML переформатируется, комментарии не сохраняются. Исходник остаётся в config.toml.bak.': 'Saving reformats TOML and removes comments. The original remains in config.toml.bak.',
  'Все настройки доступны в форме, включая необязательные поля. Значения по умолчанию загружаются с сервера. Изменения применяются после перезапуска.': 'All settings are available in the form, including optional fields. Defaults are loaded from the server. Changes apply after restart.',
  'Конфигурация загружается только по запросу. Пароли, токены и заголовки скрыты. Значения не сохраняются в хранилище браузера.': 'Configuration is loaded only on request. Passwords, tokens and headers are masked. Values are not saved in browser storage.',
  'Конфигурация изменилась на сервере. Ваши изменения сохранены в форме. Загрузите актуальную версию, чтобы отбросить их, или перенесите правки вручную.': 'Configuration changed on the server. Your edits remain in the form. Reload to discard them, or transfer your changes manually.',
  'Включить {0}': 'Enable {0}',
  'Добавить {0}': 'Add {0}',
  'Удалить {0}': 'Remove {0}',
  '{0} имя': '{0} name',
  'Имя': 'Name',
  'Необязательное поле; пустое значение отключает настройку.': 'Optional; leave empty to unset.',

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

// Structured system errors retain their parts across language changes.
export class LocalizedError extends Error {
  readonly parts:string[]
  constructor(parts:string[]) {super(parts.map(part=>t(part)).join(' '));this.parts=parts}
  localizedMessage() {return this.parts.map(part=>t(part)).join(' ')}
}
