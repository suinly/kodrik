# Кодрик — чат с вкладкой

Распакованное расширение Chrome 123+ без сборки и дополнительных зависимостей.
Использует существующий веб-чат Кодрика: общая история, потоковые ответы, инструменты,
подтверждения команд и вход через обычную вкладку.

## Установка

1. Соберите и разверните **эту версию сервера** Кодрика. Старый сервер запрещает
   iframe и не поддерживает передачу контекста:
   ```sh
   npm ci --prefix web
   npm run build --prefix web
   cargo build --release --locked
   ```
   Перезапуск работающего gateway выполняйте отдельно вашим обычным способом.
2. Откройте `chrome://extensions`, включите «Режим разработчика» и нажмите
   «Загрузить распакованное расширение». Выберите каталог `extensions/chrome`.
3. Закрепите значок Кодрика. В настройках Chrome выберите отображение боковой панели
   справа, если она у вас открывается слева.
4. Нажмите значок. Введите **базовый адрес** Кодрика, например
   `https://kodrik.example.com/` или `https://example.com/kodrik/`, без `app/chat`.
   Для локального сервера допустим `http://127.0.0.1:PORT/` или localhost.
5. Разрешите доступ к адресу Кодрика. Нажмите «Войти ↗» и выполните обычный вход.
   После входа вернитесь к панели: «Настройки» → «Подключить».
6. Откройте статью, нажмите значок расширения на этой вкладке и задайте вопрос.
   Если после переключения вкладки Chrome не разрешает чтение, нажмите значок снова
   либо «Разрешить чтение сайтов». Второй вариант даёт постоянный доступ к HTTP(S)
   сайтам; расширение читает текст при первом сообщении диалога, по запросу агента или при явном
   раскрытии предпросмотра контекста.

## Интерфейс

- Кнопка «Настройки подключения» в самой панели расширения появляется, если чат
  не ответил за 8 секунд, и скрывается после подключения. В настройках можно изменить адрес,
  открыть «Войти ↗» для введённого адреса и подключиться заново.
- В верхней строке — меню и инструменты чата, без названия текущего разговора.
  Шестерёнка слева от поиска открывает модальное окно настроек подключения.
  В новом пустом чате поиск появляется после создания разговора.
- Над полем ввода — название вкладки без переключателя контекста. Нажмите название,
  чтобы раскрыть вверх свежий текст страницы. Предпросмотр читает страницу локально,
  но не отправляет вопрос агенту.
- При недоступном тексте отображается ошибка и, при необходимости, кнопка
  «Разрешить чтение сайтов».

## Что передаётся

Браузерные инструменты доступны только для конкретного запроса, отправленного
через подключённое расширение. Сервер проверяет живую привязку панели к пользователю
и диалогу; URL-параметр боковой панели сам по себе доступа не даёт. Обычные сообщения
из веб-чата, Telegram и CLI не получают эти инструменты, даже если панель остаётся
открытой или в истории уже есть контекст вкладки.

- Только **первое сообщение диалога** автоматически получает снимок текста страницы
  или выделенного фрагмента. Следующие сообщения, в том числе после перезагрузки
  панели, не перечитывают вкладку. Новый диалог снова получает начальный снимок.
- Инструмент `browser_context` позволяет агенту запросить свежий текст связанной
  вкладки самостоятельно. Для этого панель с этим диалогом должна оставаться открытой.
  Запрос не выполняет действия на сайте и не скачивает документы: для них возвращается
  название и тип. Полный файл автоматически прикладывается только к первому сообщению.
- Адрес, заголовок, текст, источник (`page` / `selection`), время и признак усечения
  передаются отдельным объектом `browser_context` — до 100 000 байт UTF-8 в JSON.
  TXT-файл не создаётся; контекст не показывается внутри сообщений истории.
- Снимок сохраняется в серверной истории и передаётся настроенному провайдеру модели
  отдельным текстовым блоком недоверенных данных, не инструкциями или разрешением
  на действия. Это не абсолютная защита от prompt injection: проверяйте важные
  действия и используйте подтверждения инструментов.
- Не начинайте диалог из приватной вкладки, если не хотите передавать её содержимое.
  Скрытые узлы, значения полей форм и редактируемые области исключаются из обычного
  захвата. Выделение может содержать личные данные.
- Фонового чтения текста нет. Метаданные вкладки обновляются без чтения содержимого.
- После неясного результата отправки повтор использует тот же запрос и снимок,
  а не новую страницу. Снимок сохраняется с ожидающим запросом; обычные файловые
  вложения после перезагрузки могут потребовать повторного выбора.

### Скриншоты, PDF и ограничения захвата

Автоматический захват поддерживает HTML/XHTML и обычные текстовые страницы.
PDF, видео, изображения, вложенные iframe, `chrome://`, Chrome Web Store и страницы
самого сервера не читаются этим механизмом. При автоматическом захвате ошибка чтения
останавливает отправку, а не молча пропускает страницу.

Изображение или PDF можно приложить вручную через скрепку: изображения — до 10 MiB,
файлы — до 20 MB, до 10 вложений суммарно до 50 MB. Для первого сообщения нужна доступная для чтения вкладка. Расширение **не**
делает скриншот вкладки, не печатает её в PDF и не скачивает открытый PDF автоматически.
Существующие `browser-tab.txt` в старых разговорах остаются без изменений.

## Безопасность и ограничения

`manifest.key` — **публичный** ключ, а не секрет. Он фиксирует ID расширения
`iiajbdommloibgimhfmoojnhknbaklih`; только этому ID сервер разрешает iframe и мост
контекста. Не удаляйте/не меняйте ключ без обновления обоих allowlist в сервере и UI.
Пароли, cookie и токены расширение не считывает и не хранит; адрес сервера хранится
в `chrome.storage.local`, не синхронизируется. Серверные cookie и проверки Origin
не изменяются. Reverse proxy не должен добавлять `X-Frame-Options: DENY/SAMEORIGIN`
или несовместимый `frame-ancestors` к маршрутам чата. Страница входа намеренно
остаётся недоступной во фрейме; открывайте её через «Войти ↗».

Подключение использует исключение Chrome для cookie во фреймах расширений с
разрешением на host сервера. Корпоративные политики и дополнительные блокировщики
могут помешать работе. В Chrome Web Store расширение не опубликовано.

## Проверки

После установки зависимостей веб-интерфейса и Chromium:

```sh
npm run build --prefix web
node --test --test-concurrency=1 extensions/chrome/tests/*.test.mjs web/tests/browser-tab.test.mjs
cargo test --locked browser_context
cargo test --locked only_chat_html_allows_extension_framing
```

Сквозной тест загружает настоящее MV3-расширение в headless Chromium и проверяет
вход через обычную вкладку, iframe, свежий контекст, предпросмотр, настройки,
компактную шапку и восстановление разговора после перезагрузки. В тестовой копии
manifest заранее разрешены только loopback-hosts. Системный UI выбора разрешений
и размещение настоящей боковой панели требуют ручной проверки в вашем Chrome.

## Чат для каждой вкладки

Открывайте панель значком расширения отдельно на каждой вкладке. Панель привязана
к этой вкладке, а не к последней активной странице. Разговор восстанавливается при
повторном открытии панели; его привязка хранится в `chrome.storage.session` на время
сеанса браузера и удаляется при закрытии вкладки. Сами разговоры остаются в общей
истории Кодрика. Состоянием открытой/закрытой панели управляет браузер через
нативные tab-specific панели. При перезапуске браузера привязки не восстанавливаются.
Настройки сервера и кнопка входа доступны под ⚙. HTTP поддержан только для
`localhost` и `127.0.0.1`, не для IPv6-литерала `[::1]`.

## Обновление установленного расширения

После обновления исходников пересоберите и разверните сервер (включая веб-ресурсы),
затем перезагрузите распакованное расширение на `chrome://extensions` и откройте
панель заново. Одной перезагрузки расширения недостаточно: API контекста, интерфейс
встроенного чата и обновление ответов при возвращении в панель находятся на сервере.

Результаты и ограничения проверок: [VERIFICATION.md](VERIFICATION.md).

## Документы и структурированный контекст

- Обычные страницы передаются как ограниченный структурированный текст: ссылки,
  таблицы, списки, заголовки, код и alt изображений; читается доступный open Shadow DOM.
  Поля ввода, редактируемые области и скрытый текст исключаются. Выделение имеет приоритет.
- Открытый HTTP(S) PDF передаётся оригинальным файлом через существующий механизм
  вложений чата, вместе с URL и названием вкладки. Это не печать веб-страницы в PDF.
- Google Docs экспортируется в Markdown, Slides — PDF, Sheets — XLSX.
  Экспорт использует авторизацию открытой вкладки; запрет скачивания не обходится.
- Предпросмотр документа показывает название/формат, не скачивает и не отправляет файл.
  Сам документ скачивается только при первом сообщении диалога.
- Лимит одного документа — 20 MB (существующий серверный лимит), не 128 MiB ChatGPT.
  Вместе с документом вкладки допускается до 10 вложений и 49 MB за отправку.
  Повторная отправка после неопределённого сетевого результата использует сохранённый
  снимок и файл, а не скачивает изменившуюся вкладку заново; после перезагрузки чата
  может потребоваться повторно выбрать файл.
- Поддержаны документы с повторяемым GET-запросом; локальные file:, blob:, POST-only PDF,
  закрытые shadow roots и содержимое iframe не поддержаны. Google-экспорт с перенаправлением
  сейчас отклоняется с ошибкой, а не возвращает оболочку редактора или страницу входа.

После обновления нужны и новые веб-ресурсы сервера, и перезагрузка расширения.
Разрешения расширения не расширены; debugger не требуется.

### Browser automation executor

`automation.mjs` exports `executeBrowserCommand(tabId, command, confirm)` and
`confirmBrowserCommand(command, tab)`. The panel accepts trusted iframe messages
`{type:'kodrik-browser-command', channel, id, command}` and replies with
`{type:'kodrik-browser-result', channel, id, result}` or `error` (string).
Only the panel's bound tab is eligible. Message IDs are single-use; mutations are
never retried. The panel rejects concurrent commands and caps IDs at 10,000 per
panel lifetime (reload to reset).

Commands: `snapshot`; `click` with `snapshot_id, element_id`; `fill` additionally
with string `value`; `scroll` with `snapshot_id`, optional `element_id`, and `x,y`
pixels (each at most ±10,000); `evaluate` with `snapshot_id, code` (JavaScript
function body, e.g. `return document.title;`) and optional `timeout_ms` (1–10,000).
Snapshots return `{snapshot_id,url,title,text,elements}`. JSON results are limited
to 100,000 UTF-8 bytes; text is capped at 5,000 characters, elements at 150.
Field values are omitted; password/file filling is blocked. Text uses the existing
privacy-filtered extractor. Element tokens cover the top document's light DOM
only, not iframe or shadow-tree controls.

All non-snapshot commands require an extension-owned DOM confirmation dialog that
expires after 55 seconds. Document and snapshot are checked again after approval;
Chrome `documentIds` bind injections to the exact original document. MAIN-world
JavaScript obeys page CSP and fails explicitly if evaluation is blocked. Promise
timeouts stop waiting, **not execution**: asynchronous side effects may continue,
and synchronous infinite JavaScript cannot be interrupted by this timeout. Do not
retry timed-out mutations. Snapshots and tokens are ephemeral and lost on reload.

Automation follow-up safety contract:
- Snapshot target names exclude editable/hidden descendants and invisible ancestors.
  Target name, link destination, input type and form destination are fingerprinted;
  changed targets require a new snapshot. Confirmation displays the human target
  name and destination, not just its token.
- `executeBrowserCommand(tabId, command, confirm, {forbiddenOrigin, signal})`
  rejects the forbidden origin inside isolated checks/injections and MAIN evaluation.
  Form-only pages may return empty text while retaining actionable elements.
- Trusted `{type:'kodrik-browser-cancel',channel,id}` aborts pending confirmation.
  After approval the panel sends `{type:'kodrik-browser-check',channel,id}`;
  the bridge must reply `{type:'kodrik-browser-check-result',channel,id,allowed:true}`
  within two seconds. Missing/negative responses fail closed. The bridge must verify
  that the backend request is still current before approving. This cannot make a
  distributed backend check and browser execution atomic.
- An extension-side 15-second watchdog bounds waiting for MAIN injection, including
  synchronous page hangs. A timeout reports uncertain execution, invalidates the
  snapshot and releases the executor; it does NOT cancel JavaScript. Execution
  errors likewise require a fresh snapshot before another mutation.
