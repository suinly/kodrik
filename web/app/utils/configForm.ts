import {t} from './i18n.ts'

// Explicit field metadata follows src/config.rs. Defaults are supplied by the server,
// except for newly added map entries which have not been through the server yet.
export type ConfigValue = string | number | boolean | null | ConfigValue[] | {[key:string]:ConfigValue}
export type ConfigValues = {[key:string]:ConfigValue}
export type ConfigField = {
  kind:'string'|'number'|'boolean'|'object'|'map'|'array'
  optional?:boolean
  advanced?:boolean
  label?:string
  description?:string
  secret?:boolean
  min?:number
  max?:number
  step?:number|'any'
  fields?:Record<string,ConfigField>
  item?:ConfigField
  allowedKeys?:readonly string[]
  initial?:ConfigValue
}
const string=(optional=false):ConfigField=>({kind:'string',optional})
const number=(optional=false,min=0,max=Number.MAX_SAFE_INTEGER):ConfigField=>({kind:'number',optional,min,max})
const rate:ConfigField={kind:'number',optional:true,min:0,step:'any',description:'Необязательная цена в долларах США за миллион токенов для оценки стоимости.'}
const boolean:ConfigField={kind:'boolean'}
const secret:ConfigField={kind:'string',secret:true}
const object=(fields:Record<string,ConfigField>,optional=false):ConfigField=>({kind:'object',fields:Object.fromEntries(Object.entries(fields).map(([key,field])=>[key,{...field}])),optional})
const map=(item:ConfigField,initial:ConfigValue):ConfigField=>({kind:'map',item:{...item},initial})
const rates:ConfigField={...object({input_usd_per_million:rate,cached_input_usd_per_million:rate,output_usd_per_million:rate},true),advanced:true}
const reasoningEffort:ConfigField={...string(true),description:'Необязательный уровень рассуждений: none, minimal, low, medium, high, xhigh или max. Поддержка зависит от модели; пустое поле не отправляет параметр.'}
export const configSchema=object({
  owner:object({interface:string(),user_id:number(false,1)},true),
  system_prompt:string(true),
  web:object({
    enabled:boolean,
    base_url:{...string(true)},
    max_event_streams_per_user:{...number(false,1,64)},
    listen:{...string(),advanced:true},
    trusted_proxies:{kind:'array',item:string(),advanced:true},
  }),
  companion:object({
    learning_enabled:boolean,proactive_enabled:boolean,
    timezone:{...string()},
    proactive_hour:{...number(false,0,23)},
    quiet_start:{...number(false,0,23)},
    quiet_end:{...number(false,0,23)},
    interface:{...string(true),advanced:true},chat_id:{...number(true,1),advanced:true},
  }),
  sessions:object({
    restart_limit:number(false,0,4294967295),
    compact_threshold_tokens:{...number(),advanced:true},
    compact_background_margin_tokens:{...number(),advanced:true},
  }),
  research:{...object({
    searxng_url:string(true),degoog_url:string(true),
    obscura_path:{...string(),advanced:true},
  })},
  provider:map(object({
    type:string(),api_key:secret,api_base:{...string(true),advanced:true},
    tool_discovery:{...string(),advanced:true,description:'Протокол обнаружения инструментов: compatible (по умолчанию) — постоянные функции поиска и вызова; native — нативный tool_search для поддерживаемого маршрута Responses API. После изменения нужен перезапуск.'},
    model:{...string(true),description:'Точный идентификатор модели для прежней конфигурации. Для нового подключения задайте его в профиле модели.'},
    reasoning_effort:reasoningEffort,rates,
  }),{type:'openai',api_key:'',api_base:null,tool_discovery:'compatible'}),
  model:{...map(object({
    provider:{...string(),label:'Провайдер',description:'Имя существующего подключения из [provider], например openai для [provider.openai].'},
    model:{...string(),label:'Идентификатор модели в API',description:'Точный идентификатор модели, принимаемый этим API, а не произвольное название подключения.'},
    reasoning_effort:reasoningEffort,rates,
  }),{provider:'',model:''}),allowedKeys:['fast','smart','deep','vision'],label:'Профиль модели',description:'Роли профилей: fast — быстрые и фоновые задачи, smart — обычные диалоги, deep — сложные задачи, vision — изображения. Если задан [model.smart], «Авто» начинает со smart; в прежней конфигурации «Авто» начинает с fast.'},
  gateway:map(object({type:string(),token:secret,stream:boolean}),{type:'telegram',token:'',stream:true}),
  mcp:map(object({type:string(),url:string(),headers:map(secret,'')}),{type:'http',url:'',headers:{}}),
})

const fieldLabels:Record<string,string> = {"owner": "Владелец", "system_prompt": "Системная инструкция", "web": "Веб-сервер", "trusted_proxies": "Доверенные прокси", "enabled": "Включено", "listen": "Адрес прослушивания", "base_url": "Внешний адрес сервера", "max_event_streams_per_user": "Подключения на пользователя", "companion": "Компаньон", "learning_enabled": "Запоминать выводы из бесед", "proactive_enabled": "Ежедневное исследование интересов", "interface": "Имя Telegram-шлюза", "user_id": "ID пользователя Telegram", "chat_id": "ID личного чата Telegram", "timezone": "Часовой пояс", "proactive_hour": "Не раньше этого часа, 0–23", "quiet_start": "Начало тихих часов, 0–23", "quiet_end": "Конец тихих часов, 0–23", "sessions": "Сессии", "restart_limit": "Порог последовательных сбоев выполнения", "compact_threshold_tokens": "Порог сжатия истории, токены", "compact_background_margin_tokens": "Запас для фонового сжатия, токены", "research": "Поиск и чтение сайтов", "searxng_url": "Адрес SearXNG", "degoog_url": "Адрес DeGoog", "obscura_path": "Путь к Obscura", "provider": "Провайдеры ИИ", "type": "Тип API / транспорта", "api_key": "Ключ API", "api_base": "Базовый адрес API", "model": "Модель", "gateway": "Шлюзы Telegram", "token": "Токен бота", "stream": "Показывать ответ по мере генерации", "mcp": "Серверы MCP", "url": "Адрес сервера", "headers": "HTTP-заголовки"}
export const configLabel=(key:string)=>t(fieldLabels[key] ?? key)

Object.assign(fieldLabels, {
  reasoning_effort:'Уровень рассуждений',
  tool_discovery:'Обнаружение инструментов',
  rates:'Цены за токены',input_usd_per_million:'Входные токены, USD за 1 млн',
  cached_input_usd_per_million:'Кэшированные входные токены, USD за 1 млн',
  output_usd_per_million:'Выходные токены, USD за 1 млн',
})

// Help text describes runtime behavior, not literal translations of config keys.
configSchema.fields!["owner"]!.fields!["interface"]!.description="Имя настроенного шлюза из раздела Telegram, а не имя бота или пользователя."
configSchema.fields!["owner"]!.fields!["user_id"]!.description="Положительный числовой ID владельца в Telegram. Это не username и не ID группы."
configSchema.fields!["system_prompt"]!.description="Заменяет базовую системную инструкцию агента владельца. Не применяется к изолированным чатам. Для новых снимков контекста нужны перезапуск и новая сессия."
configSchema.fields!["web"]!.fields!["enabled"]!.description="Запускает HTTP-сервер страниц и веб-чата в режиме шлюза."
configSchema.fields!["web"]!.fields!["base_url"]!.description="Внешний URL сервера, включая префикс пути. Обязателен при включённом сервере. HTTPS; HTTP разрешён только для localhost и loopback."
configSchema.fields!["web"]!.fields!["listen"]!.description="Числовой IP-адрес и порт HTTP-сервера, например 127.0.0.1:8080 или 0.0.0.0:8080. Это не внешний URL."
configSchema.fields!["web"]!.fields!["trusted_proxies"]!.description="Точные IP-адреса прокси, от которых принимается X-Forwarded-For для определения IP клиента. Пустой список не доверяет этому заголовку."
configSchema.fields!["web"]!.fields!["max_event_streams_per_user"]!.description="Лимит одновременных SSE-подписок на обновления чатов одного пользователя во всех вкладках и расширении: 1–64. Не ограничивает число выполняемых задач. Общий лимит сервера — 64."
configSchema.fields!["companion"]!.fields!["learning_enabled"]!.description="После содержательных успешных бесед извлекает устойчивые выводы для памяти компаньона. Не обучает веса модели."
configSchema.fields!["companion"]!.fields!["proactive_enabled"]!.description="Разрешает не более одной попытки исследования интересов в день при работающем шлюзе, наличии активного интереса и не менее 10 минут без активности. Не отключает отдельно запланированные пробуждения."
configSchema.fields!["companion"]!.fields!["timezone"]!.description="Часовой пояс IANA для текущего времени агента, ежедневного исследования и тихих часов, например Europe/Moscow."
configSchema.fields!["companion"]!.fields!["proactive_hour"]!.description="Самый ранний местный час ежедневной попытки исследования, не точное время отправки. Дополнительно учитываются тихие часы и активность."
configSchema.fields!["companion"]!.fields!["quiet_start"]!.description="Местный час начала запрета доставки инициативных сообщений и результатов пробуждений. Обычные ответы не блокируются. Интервал может переходить через полночь."
configSchema.fields!["companion"]!.fields!["quiet_end"]!.description="Местный час окончания тихого интервала: начиная с него доставка разрешена. Должен отличаться от часа начала."
configSchema.fields!["companion"]!.fields!["interface"]!.description="Имя Telegram-шлюза для инициативных сообщений. Заполните вместе с ID личного чата. Если оба поля пусты, используется личный чат владельца."
configSchema.fields!["companion"]!.fields!["chat_id"]!.description="Положительный ID личного Telegram-чата получателя. Заполните вместе с именем шлюза; бот должен иметь возможность писать в этот чат."
configSchema.fields!["sessions"]!.fields!["restart_limit"]!.description="Число последовательных аварийных сбоев одного этапа задачи до приостановки автоматического восстановления. Переход к следующему этапу сбрасывает счётчик. 0 отключает автовосстановление. Не управляет простоем, темой диалога или перезапуском шлюза."
configSchema.fields!["sessions"]!.fields!["compact_threshold_tokens"]!.description="При такой оценке входного контекста старые сообщения в контексте модели заменяются сводкой; исходная история сохраняется. Это не лимит модели. 0 отключает автоматическое сжатие; иначе минимум 4096 токенов."
configSchema.fields!["sessions"]!.fields!["compact_background_margin_tokens"]!.description="За столько токенов до порога начинается подготовка сводки в фоне. При включённом сжатии значение меньше порога. 0 отключает только фоновую подготовку, не сжатие при достижении порога."
configSchema.fields!["research"]!.fields!["searxng_url"]!.description="Базовый HTTP(S) URL SearXNG, без /search: этот путь добавляется автоматически. Не задавайте одновременно с DeGoog."
configSchema.fields!["research"]!.fields!["degoog_url"]!.description="Базовый HTTP(S) URL DeGoog, без /api/search: этот путь добавляется автоматически. Не задавайте одновременно с SearXNG."
configSchema.fields!["research"]!.fields!["obscura_path"]!.description="Путь к исполняемому файлу Obscura или имя в PATH процесса шлюза. Это не команда с аргументами."
configSchema.fields!["provider"]!.description="Именованные подключения к API. Один провайдер может использоваться в нескольких профилях. Прежние настройки с моделями в [provider], включая system_one и primary, по-прежнему работают."
configSchema.fields!["provider"]!.item!.fields!["type"]!.description="openai — протокол OpenAI Responses API, в том числе совместимый сервер. Тип доступен только для чтения."
configSchema.fields!["provider"]!.item!.fields!["api_key"]!.description="Ключ доступа к выбранному API. Требуется непустое значение."
configSchema.fields!["provider"]!.item!.fields!["api_base"]!.description="Базовый URL OpenAI-совместимого Responses API. Пустое значение использует стандартный адрес клиента OpenAI."
configSchema.fields!["gateway"]!.item!.fields!["type"]!.description="telegram — подключение бота через Telegram Bot API. Тип доступен только для чтения."
configSchema.fields!["gateway"]!.item!.fields!["token"]!.description="Токен Telegram-бота, выданный BotFather. Не пароль аккаунта Telegram."
configSchema.fields!["gateway"]!.item!.fields!["stream"]!.description="Публикует и обновляет предварительный текст ответа во время генерации. При выключении готовый ответ отправляется после завершения."
configSchema.fields!["mcp"]!.item!.fields!["type"]!.description="http — транспорт Streamable HTTP для инструментов MCP. stdio и старый HTTP+SSE не поддерживаются."
configSchema.fields!["mcp"]!.item!.fields!["url"]!.description="Полный HTTP(S) URL точки подключения MCP, например https://example.com/mcp."
configSchema.fields!["mcp"]!.item!.fields!["headers"]!.description="HTTP-заголовки запросов к этому MCP-серверу, например Authorization. Значения могут содержать секреты."
configSchema.fields!["research"]!.description="Задайте один поисковый сервис: SearXNG или DeGoog. Без него инструменты веб-поиска и чтения страниц не подключаются."
configSchema.fields!["mcp"]!.item!.fields!["headers"]!.item!.description="Значение HTTP-заголовка. Передаётся серверу как есть, включая префикс Bearer, если он нужен."
