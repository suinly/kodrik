use super::{
    Settings,
    auth::AuthStore,
    chat::Chat,
    server::{router_with_chat, serve},
};
use crate::{
    agent::{
        Agent, AgentEvent, AgentRunner, AssistantMessage, ContextBuilder, LlmContext,
        llm::LlmProvider,
        message::{Message, ToolCall},
        session::{SessionStore, SqliteSessionStore},
        tools::{Tool, ToolRegistry, UpdatePlanTool},
    },
    config::WebConfig,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;

#[path = "chat_subagents_tests.rs"]
mod subagents;

struct FakeLlm;

#[async_trait::async_trait]
impl LlmProvider for FakeLlm {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if context
            .instructions
            .starts_with("Generate a conversation title")
        {
            assert!(tools.is_empty());
            assert!(progress.is_none());
            let Message::User(input) = &context.messages[0] else {
                panic!()
            };
            let source: Value = serde_json::from_str(&input.content)?;
            assert!(source.as_array().unwrap().len() <= 6);
            assert!(
                source
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|message| message["content"].as_str().unwrap().chars().count() <= 1000)
            );
            assert!(
                source
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|message| message.get("images").is_none())
            );
            let topic = source[0]["content"]
                .as_str()
                .filter(|text| !text.is_empty())
                .or_else(|| source[0]["files"][0].as_str())
                .unwrap_or("Фото");
            if topic == "title unavailable" {
                anyhow::bail!("title provider unavailable");
            }
            if topic == "title pending" {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                if topic == "delayed title" { 2000 } else { 100 },
            ))
            .await;
            return Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: format!("Обсуждение: {}", topic.chars().take(50).collect::<String>()),
                })],
                None,
            ));
        }
        let text = context
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(user) => Some(user.content.as_str()),
                _ => None,
            })
            .unwrap_or("");
        if context.instructions.contains("## Delegated task") {
            if text == "Проверить авторизацию" {
                return std::future::pending().await;
            }
            if text == "Проверить тесты" {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                return Ok((
                    vec![Message::Assistant(AssistantMessage {
                        content: "Проверка тестов завершена".into(),
                    })],
                    None,
                ));
            }
        }
        if text == "subagents" {
            let first = context.messages.iter().find_map(|message| {
                if let Message::ToolResult(result) = message {
                    result.content.output["id"].as_str().map(str::to_owned)
                } else {
                    None
                }
            });
            let messages = if let Some(id) = first {
                if context.messages.iter().any(|message| matches!(message,Message::ToolResult(result) if result.content.output["status"]=="stopped")) {
                    vec![Message::Assistant(AssistantMessage {content:"Дочерняя задача остановлена".into()})]
                } else {
                    vec![Message::ToolCall(ToolCall {call_id:format!("wait-{}",context.messages.len()),name:"wait_agent".into(),
                        arguments:json!({"agent_id":id,"timeout_ms":10000})})]
                }
            } else {
                ["Проверить авторизацию", "Проверить тесты"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, task)| {
                        Message::ToolCall(ToolCall {
                            call_id: format!("child-{index}"),
                            name: "spawn_agent".into(),
                            arguments: json!({"task":task}),
                        })
                    })
                    .collect()
            };
            return Ok((messages, None));
        }
        if [
            "componentsui",
            "formsui",
            "viewsui",
            "datesui",
            "gridui",
            "rangeui",
        ]
        .contains(&text)
            && !matches!(context.messages.last(), Some(Message::ToolResult(_)))
        {
            let (body, sources, actions, slug) = if text == "rangeui" {
                (
                    r#"# Коридор оценок

::u-chart{type="line" :data="sources.days" index="date" :range='{"min":"low","max":"high"}' :categories='[{"key":"low","label":"Минимум"},{"key":"high","label":"Максимум"},{"key":"target","label":"Ориентир","color":"warning"}]' aria-label="Коридор калорий" :height="260"}
::

::u-form{submit-action="edit_range"}
:::u-form-field{name="high" label="Верхняя оценка"}
::::u-input-number
::::
:::
:::u-button{type="submit" label="Сохранить оценку"}
:::
::"#,
                    json!([{"id":"days","data_json":null,"query":{"mode":"query","dataset":"demo_ranges","limit":50}}]),
                    json!([{"id":"edit_range","dataset":"demo_ranges","operation":"update","fields":["high"],"record_id":"c"}]),
                    "range-chart",
                )
            } else if text == "gridui" {
                (
                    "# Сетка КБЖУ\n\n::columns{count=\"4\"}\n:::card{title=\"Калории\"}\n1498 ккал\n:::\n:::card{title=\"Белки\"}\n24 г\n:::\n:::card{title=\"Жиры\"}\n102 г\n:::\n:::card{title=\"Углеводы\"}\n120 г\n:::\n::",
                    json!([]),
                    json!([]),
                    "kbju-grid",
                )
            } else if text == "datesui" {
                (
                    r#"# Форматы дат

::data-table{source="days" title="История" :columns='[{"accessorKey":"date","header":"День","dateFormat":"short"},{"accessorKey":"kcal","header":"Энергия, ккал"}]'}
::

::u-table{:data="sources.days" caption="Нативная таблица" :columns='[{"accessorKey":"date","header":"Дата записи","dateFormat":"short"},{"accessorKey":"kcal","header":"Калории"}]' :initial-state='{"sorting":[{"id":"date","desc":false}]}' }
#kcal-cell
{{ slot.row.original.kcal }} — :date-value{:value="slot.row.original.date" :date-format='{"day":"numeric","month":"long"}'}
::

::u-card
Дата в тексте: :date-value{:value="sources.days.0.date" date-format="long"}.

Дата с часовым поясом: :date-value{value="2026-10-21T23:30:00Z" :date-format='{"timeZone":"Europe/Moscow"}'}.

Пустая дата: :date-value{:value="null"}.
::

::data-table{source="paged" title="Приёмы" :columns='[{"accessorKey":"meal","header":"Блюдо"},{"accessorKey":"date","header":"Когда","dateFormat":"short"}]'}
::"#,
                    json!([
                        {"id":"days","data_json":"[{\"date\":\"2026-01-02\",\"kcal\":10},{\"date\":\"2025-12-31\",\"kcal\":20},{\"date\":\"2026-02-01\",\"kcal\":30}]","query":null},
                        {"id":"paged","data_json":null,"query":{"mode":"query","dataset":"demo_nutrition","limit":2}}
                    ]),
                    json!([]),
                    "date-formats",
                )
            } else if text == "viewsui" {
                (
                    r#"# КБЖУ с представлениями

::u-chart{type="line" :data="views.week" index="date" :categories='[{"key":"protein","label":"Белки","color":"primary"},{"key":"fat","label":"Жиры","color":"success"},{"key":"carbs","label":"Углеводы","color":"warning"}]' aria-label="КБЖУ за неделю" :height="220"}
::

::u-progress{:model-value="views.latest.kcal" :max="views.latest.target" aria-label="Калории"}
::

::u-progress{:segments="views.portions" :max="100" aria-label="Составной белок" status}
::

::u-table{:data="views.week"}
::

::u-form{submit-action="edit_day"}
:::u-form-field{name="kcal" label="Калории дня"}
::::u-input-number
::::
:::
:::u-form-field{name="protein" label="Белки дня"}
::::u-input-number
::::
:::
:::u-button{type="submit" label="Сохранить день"}
:::
::"#,
                    json!([{"id":"days","data_json":null,"query":{"mode":"query","dataset":"demo_views","last_days":7,"timezone":"UTC","limit":200}}]),
                    json!([{"id":"edit_day","dataset":"demo_views","operation":"update","fields":["kcal","protein"],"record_id":"d0"}]),
                    "data-views",
                )
            } else if text == "componentsui" {
                (
                    r#"# Компоненты Nuxt UI

Код: `value`.

::u-card{class="text-center" id="styled-card"}
#header
Заголовок слота
#default
:::u-badge{label="Nuxt UI"}
:::
::

::u-tabs{:items='[{"label":"Первый","slot":"first"},{"label":"Второй","slot":"second"}]'}
#first
Первая вкладка
#second
Вторая вкладка
::

::u-select{:items='["Альфа","Бета"]' placeholder="Выберите вариант"}
::

::u-carousel{:items='["Первый слайд","Второй слайд"]'}
:::u-card
{{ slot.item }}
:::
::

::u-checkbox{label="Проверенный флажок" :model-value="false"}
::

::u-input{placeholder="Локальное поле" model-value="Исходное"}
::

::u-calendar{default-value="2026-10-20"}
::

::u-modal{title="Локальное окно" description="Проверка модального окна"}
:::u-button{label="Открыть окно"}
:::
#body
Содержимое окна
::

::u-table{:data="sources.items"}
#item-cell
:::u-badge{:label="slot.row.original.item"}
:::
::

::u-chart{type="bar" :data="sources.items" index="item" :categories='["value"]' aria-label="Столбчатый график"}
::

::line-chart{source="single" x="item" y="value" label="Одна точка"}
::

::line-chart{source="empty" x="item" y="value" label="Пустой график"}
::"#,
                    json!([
                        {"id":"items","query":null,"data_json":"[{\"item\":\"Альфа\",\"value\":2},{\"item\":\"Бета\",\"value\":5}]"},
                        {"id":"single","query":null,"data_json":"[{\"item\":\"Единственное значение\",\"value\":0}]"},
                        {"id":"empty","query":null,"data_json":"[]"}
                    ]),
                    json!([]),
                    "components",
                )
            } else {
                (
                    r#"# Питание с формами

::metric{label="Всего калорий" source="totals" field="kcal"}
::

::u-form{submit-action="add"}
:::u-form-field{label="Дата нового приёма" name="date"}
::::u-input{type="date"}
::::
:::
:::u-form-field{label="Новое блюдо" name="meal"}
::::u-input
::::
:::
:::u-form-field{label="Новые калории" name="kcal"}
::::u-input-number
::::
:::
:::u-button{type="submit" label="Добавить запись"}
:::
::

::u-form{submit-action="edit"}
:::u-form-field{label="Изменить блюдо" name="meal"}
::::u-input
::::
:::
:::u-form-field{label="Изменить калории" name="kcal"}
::::u-input-number
::::
:::
:::u-form-field{label="Дополнительное число" name="extra"}
::::u-input-number
::::
:::
:::u-form-field{label="Необязательная дата" name="optional_date"}
::::u-input{type="date"}
::::
:::
:::u-button{type="submit" label="Сохранить изменения"}
:::
::

::u-form{submit-action="add"}
:::u-form-field{label="Дата по умолчанию" name="date"}
::::u-input{type="date" default-value="2026-10-23"}
::::
:::
:::u-form-field{label="Блюдо по умолчанию" name="meal"}
::::u-input{model-value="Untouched defaults"}
::::
:::
:::u-form-field{label="Калории по умолчанию" name="kcal"}
::::u-input-number{:default-value="50"}
::::
:::
:::u-button{type="submit" label="Сохранить значения по умолчанию"}
:::
::

::data-table{source="meals"}
::"#,
                    json!([
                        {"id":"meals","data_json":null,"query":{"mode":"query","dataset":"demo_forms","limit":50}},
                        {"id":"totals","data_json":null,"query":{"mode":"summarize","dataset":"demo_forms","metrics":["kcal"]}}
                    ]),
                    json!([
                        {"id":"add","dataset":"demo_forms","operation":"insert","fields":["date","meal","kcal"],"record_id":null},
                    {"id":"edit","dataset":"demo_forms","operation":"update","fields":["meal","kcal","extra","optional_date"],"record_id":"first"}
                    ]),
                    "forms",
                )
            };
            return Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: "native-components".into(),
                    name: "show_ui".into(),
                    arguments: json!({"title":"Nuxt UI","body":body,"sources":sources,"views":if text=="viewsui" {json!([{"id":"week","source":"days","jq":"sort_by(.date)"},{"id":"latest","source":"days","jq":"sort_by(.date) | last"},{"id":"portions","source":"days","jq":"sort_by(.date) | last | [{value:(.protein-20),color:\"primary\",label:\"Основное\"},{value:20,color:\"error\",label:\"Превышение\"}]"}])} else {json!([])},"actions":actions,"slug":slug,"visibility":null}),
                })],
                None,
            ));
        }
        if ["ui", "unsafeui"].contains(&text)
            && !matches!(context.messages.last(), Some(Message::ToolResult(_)))
        {
            let body = if text == "unsafeui" {
                "::nuxt-link{to=\"javascript:alert(1)\"}\n::"
            } else {
                "# Дневник питания\n\n::columns\n:::metric{label=\"Калории\" source=\"totals\" field=\"kcal\" unit=\"ккал\"}\n:::\n:::metric{label=\"Белки\" value=\"90\" unit=\"г\"}\n:::\n::\n\n::data-table{source=\"meals\" title=\"Приёмы пищи\"}\n::\n\n::line-chart{source=\"daily\" x=\"group\" y=\"kcal\" label=\"Калории по дням\" unit=\"ккал\"}\n::\n\n::button{label=\"Добавить приём пищи\" prompt=\"Помоги добавить приём пищи\"}\n::"
            };
            let query = |mode: &str, group: Option<&str>, limit: Option<usize>| json!({"mode":mode,"dataset":"demo_nutrition","date_from":null,"date_to":null,"filters":null,"metrics":if mode=="summarize" {Some(vec!["kcal"])} else {None},"group_by":group,"limit":limit});
            return Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: "ui-call".into(),
                    name: "show_ui".into(),
                    arguments: json!({"title":"Дневник питания","body":body,"sources":[
                {"id":"meals","data_json":null,"query":query("query",None,Some(2))},
                {"id":"daily","data_json":null,"query":query("summarize",Some("day"),None)},
                {"id":"totals","data_json":null,"query":query("summarize",None,None)}],"slug":if text=="ui"{Some("nutrition")}else{None},"visibility":null}),
                })],
                None,
            ));
        }
        if text == "restart"
            && let Some(progress) = progress
        {
            progress
                .send(AgentEvent::TextDelta("Перед перезапуском".into()))
                .await?;
            std::future::pending::<()>().await;
        }
        if text == "wait" {
            if let Some(progress) = progress {
                progress
                    .send(AgentEvent::TextDelta("Жду остановки".into()))
                    .await?;
            }
            std::future::pending::<()>().await;
        }
        if text == "backoff" {
            return Err(crate::agent::llm::GenerationRetry {
                limit: 3,
                retry_after: Some(std::time::Duration::from_secs(30)),
                error: anyhow::anyhow!("rate limited"),
            }
            .into());
        }
        if text == "slowtool" && !matches!(context.messages.last(), Some(Message::ToolResult(_))) {
            return Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: "slow-call".into(),
                    name: "slow_tool".into(),
                    arguments: json!({}),
                })],
                None,
            ));
        }
        if text == "fail" {
            anyhow::bail!("test provider failure");
        }
        if text == "tools" && !matches!(context.messages.last(), Some(Message::ToolResult(_))) {
            return Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: "plan-call".into(),
                    name: "update_plan".into(),
                    arguments: json!({"plan":[{"step":"Проверить чат", "status":"completed"}]}),
                })],
                None,
            ));
        }
        if let Some(progress) = progress {
            if text == "retry" {
                progress
                    .send(AgentEvent::TextDelta("Неверная попытка".into()))
                    .await?;
                progress
                    .send(AgentEvent::Retrying("Повторяю запрос".into()))
                    .await?;
            }
            progress.send(AgentEvent::TextDelta("При".into())).await?;
            progress.send(AgentEvent::TextDelta("вет".into())).await?;
        }
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: "Привет!".into(),
            })],
            Some(42),
        ))
    }
}

struct SlowTool(std::path::PathBuf);
#[async_trait::async_trait]
impl Tool for SlowTool {
    fn name(&self) -> String {
        "slow_tool".into()
    }
    fn description(&self) -> String {
        "Test slow effect".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: Value) -> anyhow::Result<crate::agent::tools::ToolOutput> {
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.0)?,
            "effect"
        )?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        Ok(json!({"effects":std::fs::read_to_string(&self.0)?.lines().count()}).into())
    }
}

pub(super) async fn chat(data: &std::path::Path) -> Chat {
    chat_with_ui(data, None).await
}

async fn chat_with_ui(
    data: &std::path::Path,
    publication: Option<(std::path::PathBuf, Settings)>,
) -> Chat {
    let datasets = crate::agent::datasets::DatasetStore::open(data.join("datasets")).unwrap();
    let sessions = SqliteSessionStore::open(data.join("owner-sessions")).unwrap();
    let agent = Agent::new(
        Arc::new(FakeLlm),
        ToolRegistry::default()
            .register(Arc::new(UpdatePlanTool))
            .register(Arc::new(crate::agent::tools::ShowUiTool::new(
                datasets,
                publication,
            )))
            .register(Arc::new(SlowTool(data.join("effects.txt")))),
        ContextBuilder::new("test"),
    );
    let subagents =
        crate::agent::subagents::Subagents::new(Arc::new(agent.child_agent()), sessions.clone())
            .await
            .unwrap();
    let owner = AgentRunner::new(
        Arc::new(agent.with_subagents(subagents)),
        Arc::new(sessions),
    );
    Chat::new(&owner, data).await.unwrap()
}

#[tokio::test]
async fn dashboard_links_survive_owner_login_redirect() {
    let f = Fixture::new().await;
    for query in [
        "page=nutrition",
        "webhook=incident-hook",
        "conversation=test-session",
    ] {
        let response = f
            .client
            .get(format!("{}chat/?{query}", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        let login = reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        assert_eq!(
            login
                .query_pairs()
                .find(|(key, _)| key == "return_to")
                .unwrap()
                .1,
            format!("/kodrik/chat/?{query}")
        );
        let redirect = f
            .client
            .get(format!("{}chat?{query}", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(
            redirect.headers()["location"],
            format!("/kodrik/chat/?{query}")
        );
    }
}

struct Fixture {
    data: tempfile::TempDir,
    base: String,
    origin: String,
    cookie: String,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    async fn new() -> Self {
        Self::configured(None, "127.0.0.1:0").await
    }
    async fn configured(storage: Option<&std::path::Path>, listen: &str) -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = storage.unwrap_or(data.path());
        let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let base = format!("{origin}/kodrik/");
        let settings = Settings::from_config(&WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: "127.0.0.1:0".into(),
            base_url: Some(base.clone()),
        })
        .unwrap()
        .unwrap();
        let auth = AuthStore::new(root);
        if !root.join("web/auth.json").exists() {
            auth.set_password("preview-password").unwrap();
        }
        let token = auth
            .login("preview-password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = super::auth::session_cookie(&settings, &token, false)
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let datasets = crate::agent::datasets::DatasetStore::open(root.join("datasets")).unwrap();
        let mut owner = chat_with_ui(root, Some((root.to_owned(), settings.clone()))).await;
        if storage.is_some() {
            auth.configure_owner(Some(42)).unwrap();
            auth.grant_test_user(7).unwrap();
            let config = super::chat::user_tests::user_config();
            let app = crate::bootstrap::build_app_at(&config, &root.join("users/42"), false)
                .await
                .unwrap();
            let users = Arc::new(crate::users::UserSpaces::new(&config, root, app));
            let user = users.app(7).await.unwrap();
            let store = user.agent_runner.sqlite_store().unwrap();
            let id = crate::agent::session::SessionId::new("telegram:main:7");
            if store.load(&id).await.unwrap().is_none() {
                let mut session = crate::agent::session::Session::new(id);
                session.push(Message::User(crate::agent::UserMessage {
                    content: "История из Telegram".into(),
                    files: vec![],
                    images: vec![],
                }));
                store.save(&session).await.unwrap();
            }
            owner = owner.with_users(users);
        }
        let mut app =
            super::server::router_with_resources(settings, root, Some(owner), Some(datasets));
        if storage.is_some() {
            auth.register_telegram_bot("main", "TestBot").unwrap();
            app = app.route("/kodrik/__test/telegram-login", axum::routing::post(move |axum::Json(input): axum::Json<Value>| {
                let auth = auth.clone();
                async move {
                    let id = input["id"].as_str().unwrap();
                    let user = input["user"].as_u64().unwrap_or(7);
                    let now = chrono::Utc::now().timestamp();
                    auth.present_telegram_login(id,"main",user,now).unwrap();
                    axum::Json(json!({"ok":auth.decide_telegram_login(id,"main",user,input["approve"].as_bool().unwrap(),now).unwrap()}))
                }
            }));
        }
        let server = tokio::spawn(serve(listener, app));
        Self {
            data,
            base,
            origin,
            cookie,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            server,
        }
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}api/chat/{path}", self.base))
            .header("Cookie", &self.cookie)
            .header("Origin", &self.origin)
    }
    async fn create(&self) -> String {
        let response = self
            .request(reqwest::Method::POST, "sessions")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn history(&self, id: &str) -> Value {
        self.request(reqwest::Method::GET, &format!("sessions/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn idle(&self, id: &str) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let history = self.history(id).await;
                if history["busy"] == false {
                    return history;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(super) fn multipart(id: &str, request: &str, text: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in [("id", id), ("request_id", request), ("text", text)] {
        body.extend_from_slice(
            format!(
                "--chat-test\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    for (name, bytes) in files {
        body.extend_from_slice(format!("--chat-test\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--chat-test--\r\n");
    body
}

#[tokio::test]
async fn chat_slow_multipart_upload_outlives_text_request_timeout() {
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let id = f.create().await;
    let settings = Settings::from_config(&WebConfig {
        trusted_proxies: Vec::new(),
        enabled: true,
        listen: "127.0.0.1:0".into(),
        base_url: Some(f.base.clone()),
    })
    .unwrap()
    .unwrap();
    let app = router_with_chat(settings, f.data.path(), Some(chat(f.data.path()).await));
    let bytes = multipart(
        &id,
        &uuid::Uuid::new_v4().to_string(),
        "",
        &[("slow.txt", b"slow upload")],
    );
    let body = axum::body::Body::from_stream(futures::stream::once(async move {
        tokio::time::sleep(std::time::Duration::from_secs(11)).await;
        Ok::<_, std::io::Error>(bytes)
    }));
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/kodrik/api/chat/send")
                .header("Cookie", &f.cookie)
                .header("Origin", &f.origin)
                .header("Content-Type", "multipart/form-data; boundary=chat-test")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        f.idle(&id).await["messages"][0]["parts"][1]["filename"],
        "slow.txt"
    );
}

#[tokio::test]
async fn chat_nuxt_assets_use_configured_prefix_and_script_nonce() {
    let f = Fixture::new().await;
    let response = f
        .client
        .get(format!("{}chat/", f.base))
        .header("Cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    let nonce = policy
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    let html = response.text().await.unwrap();
    assert!(!html.contains("/__kodrik_chat__/"));
    assert!(html.contains("/kodrik/chat/_nuxt/"));
    assert!(html.contains("viewport-fit=cover"));
    assert!(html.contains("/kodrik/chat/manifest.webmanifest"));
    assert!(html.contains("/kodrik/chat/apple-touch-icon.png"));
    for script in html.split("<script").skip(1) {
        assert!(script.starts_with(&format!(" nonce=\"{nonce}\"")));
    }
    assert!(
        !policy
            .split("style-src")
            .next()
            .unwrap()
            .contains("'unsafe-inline'")
    );
}

#[tokio::test]
async fn chat_home_screen_assets_are_public_and_prefix_relative() {
    let f = Fixture::new().await;
    let response = f
        .client
        .get(format!("{}chat/manifest.webmanifest", f.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/manifest+json"
    );
    let manifest: Value = response.json().await.unwrap();
    assert_eq!(manifest["display"], "standalone");
    assert_eq!(manifest["start_url"], "./");
    assert_eq!(manifest["scope"], "../");
    for filename in ["logo.svg", "logo-dark.svg", "icon.svg"] {
        let response = f
            .client
            .get(format!("{}chat/{filename}", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{filename}");
        assert_eq!(response.headers()["content-type"], "image/svg+xml");
    }
    for (filename, size) in [
        ("apple-touch-icon.png", 180),
        ("icon-192.png", 192),
        ("icon-512.png", 512),
    ] {
        let response = f
            .client
            .get(format!("{}chat/{filename}", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "image/png");
        let icon = image::load_from_memory(&response.bytes().await.unwrap()).unwrap();
        assert_eq!((icon.width(), icon.height()), (size, size));
    }
    assert_eq!(
        f.client
            .get(format!("{}api/chat/sessions", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn chat_attachments_roundtrip_deduplicate_and_download_privately() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let request = uuid::Uuid::new_v4().to_string();
    let photo = crate::agent::image::tests::sample(image::ImageFormat::Png);
    let files = [
        ("notes.txt", &b"private notes"[..]),
        ("photo.png", photo.as_slice()),
    ];
    for _ in 0..2 {
        let response = f
            .request(reqwest::Method::POST, "send")
            .header("Content-Type", "multipart/form-data; boundary=chat-test")
            .body(multipart(&id, &request, "", &files))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    let view = f.idle(&id).await;
    assert_eq!(view["messages"].as_array().unwrap().len(), 2);
    let parts = view["messages"][0]["parts"].as_array().unwrap();
    assert_eq!(parts.iter().filter(|p| p["type"] == "file").count(), 2);
    for (media, bytes) in [
        ("text/plain", &b"private notes"[..]),
        ("image/png", photo.as_slice()),
    ] {
        let part = parts.iter().find(|p| p["mediaType"] == media).unwrap();
        let url = reqwest::Url::parse(&format!("{}chat/", f.base))
            .unwrap()
            .join(part["url"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            f.client.get(url.clone()).send().await.unwrap().status(),
            401
        );
        let response = f
            .client
            .get(url)
            .header("Cookie", &f.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    }
    let response = f
        .request(reqwest::Method::POST, "send")
        .header("Content-Type", "multipart/form-data; boundary=chat-test")
        .body(multipart(&id, &request, "", &[("notes.txt", b"changed")]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let directory = f.data.path().join("web/attachments").join(&id);
    assert_eq!(
        std::fs::read_dir(directory).unwrap().count(),
        1,
        "duplicate and rejected submissions must not leak files"
    );
}

#[tokio::test]
async fn chat_attachments_reject_invalid_input_without_saving_messages() {
    let f = Fixture::new().await;
    let id = f.create().await;
    for files in [
        vec![("bad.png", &b"not an image"[..])],
        vec![("bad.exe", &b"executable"[..])],
        vec![("empty.txt", &b""[..])],
        vec![("notes.txt", &b"hello"[..]); 11],
    ] {
        let response = f
            .request(reqwest::Method::POST, "send")
            .header("Content-Type", "multipart/form-data; boundary=chat-test")
            .body(multipart(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                "",
                &files,
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let response = f
        .request(reqwest::Method::POST, "send")
        .header("Origin", "https://evil.example")
        .header("Content-Type", "multipart/form-data; boundary=chat-test")
        .body(multipart(
            &id,
            &uuid::Uuid::new_v4().to_string(),
            "",
            &[("notes.txt", b"hello")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(f.history(&id).await["messages"], json!([]));
}

#[tokio::test]
async fn chat_conversation_pages_keep_pins_branches_and_old_history_search() {
    use crate::agent::session::SessionId;
    let f = Fixture::new().await;
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    for index in 0..65 {
        let mut session =
            crate::agent::session::Session::new(SessionId::new(format!("paged-{index:03}")));
        session.push(Message::User(crate::agent::UserMessage {
            content: if index == 2 {
                "Исторический маяк".into()
            } else {
                format!("Диалог {index}")
            },
            files: vec![],
            images: vec![],
        }));
        store.save(&session).await.unwrap();
    }
    for id in [
        "paged-branch",
        "companion:private",
        "telegram:main:-9",
        "telegram:main:7:guest",
    ] {
        store
            .save(&crate::agent::session::Session::new(SessionId::new(id)))
            .await
            .unwrap();
    }
    store.with_connection(|db| {
        db.execute("UPDATE sessions SET updated_at=1000+CAST(substr(id,7) AS INTEGER) WHERE id GLOB 'paged-[0-9]*'",[])?;
        db.execute("UPDATE sessions SET updated_at=2000 WHERE id='paged-branch'",[])?;
        db.execute("INSERT INTO web_chat_branches VALUES('paged-branch','paged-001','paged-001',0)",[])?;
        db.execute("INSERT INTO web_chat_selection VALUES('paged-001','paged-branch')",[])?;
        db.execute("INSERT INTO web_chat_pins VALUES('paged-000')",[])?;
        Ok(())
    }).await.unwrap();
    let first: Vec<Value> = f
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first.len(), 30);
    assert_eq!(first[0]["id"], "paged-000");
    assert_eq!(first[1]["id"], "paged-branch");
    assert_eq!(first[1]["root_id"], "paged-001");
    let mut roots = Vec::new();
    for offset in [0, 30, 60] {
        let page: Vec<Value> = f
            .request(
                reqwest::Method::GET,
                &format!("sessions?limit=30&offset={offset}"),
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page.len(), if offset == 60 { 5 } else { 30 });
        roots.extend(
            page.into_iter()
                .map(|item| item["root_id"].as_str().unwrap().to_owned()),
        );
    }
    assert_eq!(
        roots.iter().collect::<std::collections::HashSet<_>>().len(),
        65
    );
    assert_eq!(roots[2], "paged-064");
    let found: Vec<Value> = f
        .request(reqwest::Method::GET, "sessions?q=Диалог&limit=2&offset=2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0]["id"], "paged-064");
    assert_eq!(found[1]["id"], "paged-063");
    let found: Vec<Value> = f
        .request(
            reqwest::Method::GET,
            "sessions?q=Исторический%20маяк&limit=1",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], "paged-002");
    for path in [
        "sessions?limit=0",
        "sessions?limit=101",
        "sessions?offset=-1",
        "sessions?offset=18446744073709551615",
    ] {
        assert_eq!(
            f.request(reqwest::Method::GET, path)
                .send()
                .await
                .unwrap()
                .status(),
            400,
            "{path}"
        );
    }
    let last: Vec<Value> = f
        .request(reqwest::Method::GET, "sessions?limit=30&offset=65")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(last.is_empty());
}

#[tokio::test]
async fn chat_pins_persist_and_sort_before_recent_conversations() {
    let f = Fixture::new().await;
    let old = f.create().await;
    let newer = f.create().await;
    let latest = f.create().await;
    for id in [&old, &old, &newer] {
        assert_eq!(
            f.request(reqwest::Method::POST, &format!("sessions/{id}/pin"))
                .json(&json!({"pinned":true}))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
    }
    f.server.abort();
    let restored = Fixture::configured(Some(f.data.path()), "127.0.0.1:0").await;
    let listed: Value = restored
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![newer.as_str(), old.as_str(), latest.as_str()]
    );
    assert_eq!(listed[0]["pinned"], true);
    assert_eq!(listed[1]["pinned"], true);
    assert_eq!(listed[2]["pinned"], false);
    assert_eq!(
        restored
            .request(reqwest::Method::POST, &format!("sessions/{old}/pin"))
            .json(&json!({"pinned":false}))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let listed: Value = restored
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![newer.as_str(), latest.as_str(), old.as_str()]
    );
    for invalid in [
        json!({}),
        json!({"pinned":"true"}),
        json!({"pinned":true,"extra":true}),
    ] {
        assert_eq!(
            restored
                .request(reqwest::Method::POST, &format!("sessions/{old}/pin"))
                .json(&invalid)
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
    }
    assert_eq!(
        restored
            .request(
                reqwest::Method::POST,
                &format!("sessions/{}/pin", uuid::Uuid::new_v4())
            )
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        restored
            .request(reqwest::Method::POST, &format!("sessions/{old}/pin"))
            .header("Origin", "https://evil.example")
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        restored
            .request(reqwest::Method::DELETE, &format!("sessions/{newer}"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let db =
        rusqlite::Connection::open(f.data.path().join("owner-sessions/state.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM web_chat_pins", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn chat_management_preserves_versions_searches_contents_and_deletes_all_branches() {
    let fixture = Fixture::new().await;
    let id = fixture.create().await;
    let send = fixture
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"Исключительный текст"}))
        .send()
        .await
        .unwrap();
    assert_eq!(send.status(), 200);
    fixture.idle(&id).await;
    let renamed = fixture
        .request(reqwest::Method::PATCH, &format!("sessions/{id}"))
        .json(&json!({"title":"Моё название"}))
        .send()
        .await
        .unwrap();
    assert_eq!(renamed.status(), 200);
    let found: Value = fixture
        .request(reqwest::Method::GET, "sessions?q=исключительный")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["title"], "Моё название");
    assert!(
        found[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("Исключительный")
    );
    let branch = uuid::Uuid::new_v4().to_string();
    let edit = || {
        fixture
            .request(
                reqwest::Method::POST,
                &format!("sessions/{id}/messages/0/edit"),
            )
            .json(&json!({"request_id":branch,"text":"Исправленный вопрос"}))
    };
    let response = edit().send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["id"], branch);
    assert_eq!(edit().send().await.unwrap().status(), 200);
    let history = fixture.idle(&branch).await;
    assert_eq!(
        history["messages"][0]["parts"][0]["text"],
        "Исправленный вопрос"
    );
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        fixture.history(&id).await["messages"][0]["parts"][0]["text"],
        "Исключительный текст"
    );
    let view: Value = fixture
        .request(reqwest::Method::GET, &format!("sessions/{branch}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["title"], "Моё название");
    assert_eq!(view["messages"][0]["versions"], json!([id, branch]));
    assert_eq!(view["messages"][0]["version"], 1);
    assert_eq!(
        fixture
            .request(reqwest::Method::POST, &format!("sessions/{branch}/pin"))
            .json(&json!({"pinned":true}))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let list: Value = fixture
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], branch);
    assert_eq!(list[0]["root_id"], id);
    assert_eq!(list[0]["pinned"], true);
    let found: Value = fixture
        .request(reqwest::Method::GET, "sessions?q=исключительный")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found[0]["id"], id);
    assert_eq!(found[0]["pinned"], true);
    let conflict = fixture
        .request(
            reqwest::Method::POST,
            &format!("sessions/{id}/messages/0/edit"),
        )
        .json(&json!({"request_id":branch,"text":"Другой текст"}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    let missing: Value = fixture
        .request(reqwest::Method::GET, "sessions?q=несуществующий")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(missing, json!([]));
    let deleted = fixture
        .request(reqwest::Method::DELETE, &format!("sessions/{branch}"))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    for removed in [&id, &branch] {
        assert_eq!(
            fixture
                .request(reqwest::Method::GET, &format!("sessions/{removed}/run"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
}

#[tokio::test]
async fn chat_management_keeps_attachments_and_nested_versions_and_rejects_busy_changes() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let upload = f
        .request(reqwest::Method::POST, "send")
        .multipart(
            reqwest::multipart::Form::new()
                .text("id", id.clone())
                .text("text", "Исходный файл")
                .part(
                    "files",
                    reqwest::multipart::Part::bytes(b"private notes".to_vec())
                        .file_name("notes.txt"),
                ),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), 200);
    f.idle(&id).await;
    let branch = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        f.request(
            reqwest::Method::POST,
            &format!("sessions/{id}/messages/0/edit")
        )
        .json(&json!({"request_id":branch,"text":"Новая подпись"}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    let history = f.idle(&branch).await;
    let attachment = history["messages"][0]["parts"][1]["url"].as_str().unwrap();
    let bytes = f
        .client
        .get(format!(
            "{}{}",
            f.base,
            attachment.trim_start_matches("../")
        ))
        .header("Cookie", &f.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(bytes.status(), 200);
    assert_eq!(bytes.bytes().await.unwrap().as_ref(), b"private notes");
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":branch,"text":"Следующий вопрос"}))
        .send()
        .await
        .unwrap();
    f.idle(&branch).await;
    let nested = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        f.request(
            reqwest::Method::POST,
            &format!("sessions/{branch}/messages/2/edit")
        )
        .json(&json!({"request_id":nested,"text":"Уточнённый вопрос"}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    f.idle(&nested).await;
    let view: Value = f
        .request(reqwest::Method::GET, &format!("sessions/{nested}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["messages"][0]["versions"], json!([id, branch]));
    assert_eq!(view["messages"][2]["versions"], json!([branch, nested]));
    assert_eq!(view["messages"][2]["parts"][0]["text"], "Уточнённый вопрос");
    let invalid = f
        .request(
            reqwest::Method::POST,
            &format!("sessions/{nested}/messages/1/edit"),
        )
        .json(&json!({"request_id":uuid::Uuid::new_v4().to_string(),"text":"bad"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    for title in ["   ", "bad\nname", &"a".repeat(201)] {
        assert_eq!(
            f.request(reqwest::Method::PATCH, &format!("sessions/{id}"))
                .json(&json!({"title":title}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let sent = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":nested,"text":"wait"}))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.status(), 200);
    assert_eq!(
        f.request(reqwest::Method::DELETE, &format!("sessions/{id}"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        f.request(
            reqwest::Method::POST,
            &format!("sessions/{id}/messages/0/edit")
        )
        .json(&json!({"request_id":uuid::Uuid::new_v4().to_string(),"text":"busy"}))
        .send()
        .await
        .unwrap()
        .status(),
        409
    );
    assert_eq!(
        f.request(reqwest::Method::POST, &format!("sessions/{nested}/stop"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    f.idle(&nested).await;
    assert_eq!(
        f.request(reqwest::Method::DELETE, &format!("sessions/{id}"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(!f.data.path().join("web/attachments").join(&id).exists());
}

#[tokio::test]
async fn chat_delete_racing_stop_leaves_no_control_or_history_records() {
    let f = Fixture::new().await;
    for _ in 0..10 {
        let id = f.create().await;
        let deletion = f.request(reqwest::Method::DELETE, &format!("sessions/{id}"));
        let stopping = f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"));
        let (deleted, stopped) = tokio::join!(deletion.send(), stopping.send());
        assert_eq!(deleted.unwrap().status(), 204);
        assert!([202, 404].contains(&stopped.unwrap().status().as_u16()));
        let db =
            rusqlite::Connection::open(f.data.path().join("owner-sessions/state.sqlite3")).unwrap();
        for table in ["sessions", "submissions", "inbox_controls"] {
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "Deleted conversation retained {table}");
        }
    }
}

#[tokio::test]
async fn chat_editing_slash_text_generates_an_answer_instead_of_a_control_command() {
    let f = Fixture::new().await;
    let id = f.create().await;
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"Original"}))
        .send()
        .await
        .unwrap();
    f.idle(&id).await;
    for text in ["/stop", "/new"] {
        let branch = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            f.request(
                reqwest::Method::POST,
                &format!("sessions/{id}/messages/0/edit")
            )
            .json(&json!({"request_id":branch,"text":text}))
            .send()
            .await
            .unwrap()
            .status(),
            200
        );
        let history = f.idle(&branch).await;
        assert_eq!(history["messages"][0]["parts"][0]["text"], text);
        assert_eq!(history["messages"][1]["parts"][0]["text"], "Привет!");
    }
}

#[tokio::test]
async fn chat_roundtrip_tools_and_shared_owner_history() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let response = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"tools"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let accepted: Value = response.json().await.unwrap();
    assert!(accepted["submission_id"].is_string());
    let history = f.idle(&id).await;
    assert_eq!(history["busy"], false);
    assert_eq!(history["messages"][0]["parts"][0]["text"], "tools");
    assert_eq!(
        history["messages"][1]["parts"][0]["state"],
        "output-available"
    );
    assert_eq!(history["messages"][1]["parts"][1]["text"], "Привет!");
    assert_eq!(f.history(&id).await["messages"], history["messages"]);
    let restored = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    assert_eq!(restored.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn default_titles_are_marked_without_relabeling_user_authored_titles() {
    let fixture = Fixture::new().await;
    let created: Value = fixture
        .request(reqwest::Method::POST, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["title_default"], true);
    let id = created["id"].as_str().unwrap();
    for expected_default in [true, false] {
        if !expected_default {
            assert_eq!(
                fixture
                    .request(reqwest::Method::PATCH, &format!("sessions/{id}"))
                    .json(&json!({"title":"Новый диалог"}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        for path in [
            "sessions".to_owned(),
            format!("sessions/{id}"),
            format!("sessions/{id}/run"),
        ] {
            let response: Value = fixture
                .request(reqwest::Method::GET, &path)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let view = if path == "sessions" {
                &response[0]
            } else {
                &response
            };
            assert_eq!(view["title"], "Новый диалог");
            assert_eq!(view["title_default"], expected_default, "{path}");
        }
    }
}

#[tokio::test]
async fn chat_model_titles_persist_without_changing_history() {
    let f = Fixture::new().await;
    let id = f.create().await;
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"Меню на неделю"}))
        .send()
        .await
        .unwrap();
    let history = f.idle(&id).await;
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let view: Value = f
                .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if view["title"] == "Обсуждение: Меню на неделю" {
                break view;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("No generated title in snapshots");
    assert_eq!(snapshot["messages"], history["messages"]);
    let restored = Fixture::configured(Some(f.data.path()), "127.0.0.1:0").await;
    let sessions: Value = restored
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions[0]["title"], snapshot["title"]);
    restored
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"Ещё один вопрос"}))
        .send()
        .await
        .unwrap();
    restored.idle(&id).await;
    let sessions: Value = restored
        .request(reqwest::Method::GET, "sessions")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions[0]["title"], snapshot["title"]);

    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    store
        .delete(&crate::agent::session::SessionId::new(&id))
        .await
        .unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_chat_titles",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn chat_title_progress_is_separate_from_agent_work() {
    let f = Fixture::new().await;
    let id = f.create().await;
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"delayed title"}))
        .send()
        .await
        .unwrap();
    f.idle(&id).await;
    let mut revisions = Vec::new();
    for pending in [true, false] {
        let view = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let view: Value = f
                    .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if view["title_pending"] == pending {
                    break view;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(view["busy"], false);
        let sessions: Value = f
            .request(reqwest::Method::GET, "sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions[0]["title_pending"], pending);
        revisions.push(view["titles_revision"].as_i64().unwrap());
        if !pending {
            assert_eq!(view["title"], "Обсуждение: delayed title");
        }
    }
    assert!(revisions[1] > revisions[0]);
}

#[tokio::test]
async fn chat_title_failure_or_delay_does_not_block_answers() {
    let f = Fixture::new().await;
    for text in ["title unavailable", "title pending"] {
        let id = f.create().await;
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"text":text}))
            .send()
            .await
            .unwrap();
        assert_eq!(f.idle(&id).await["messages"].as_array().unwrap().len(), 2);
        let mut snapshot: Value = f
            .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snapshot["title"], text);
        if text == "title unavailable" {
            tokio::time::timeout(std::time::Duration::from_secs(4), async {
                while snapshot["title_pending"] == true {
                    assert_eq!(snapshot["busy"], false);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    snapshot = f
                        .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                }
                assert_eq!(snapshot["title_pending"], false);
            })
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
async fn chat_steers_after_tool_with_attachments_and_deduplicates_retries() {
    let f = Fixture::new().await;
    let id = f.create().await;
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"slowtool"}))
        .send()
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !f.data.path().join("effects.txt").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = uuid::Uuid::new_v4().to_string();
    let payload = json!({"id":id,"request_id":request,"text":"queued first"});
    let mut accepted = None;
    for _ in 0..2 {
        let response = f
            .request(reqwest::Method::POST, "send")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let record: Value = response.json().await.unwrap();
        if let Some(previous) = &accepted {
            assert_eq!(&record["submission_id"], previous);
        }
        accepted = Some(record["submission_id"].clone());
    }
    let photo = crate::agent::image::tests::sample(image::ImageFormat::Png);
    let attachment_request = uuid::Uuid::new_v4().to_string();
    let response = f
        .request(reqwest::Method::POST, "send")
        .header("Content-Type", "multipart/form-data; boundary=chat-test")
        .body(multipart(
            &id,
            &attachment_request,
            "",
            &[
                ("queued.txt", b"queued bytes"),
                ("queued.png", photo.as_slice()),
            ],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let view: Value = f
        .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["queued"].as_array().unwrap().len(), 2);
    assert_eq!(view["queued"][0]["text"], "queued first");
    assert_eq!(view["queued"][1]["files"], json!(["queued.txt"]));
    assert_eq!(view["queued"][1]["photos"], 1);
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    for request in [&request, &attachment_request] {
        let record = store.submission(&id, request).await.unwrap().unwrap();
        assert_eq!(record.status, crate::agent::inbox::SubmissionStatus::Queued);
        assert_eq!(record.run_id.as_deref(), view["run_id"].as_str());
        assert!(
            record.run_id.is_some(),
            "Input must belong to the active run"
        );
    }
    let history = f.idle(&id).await;
    let users: Vec<_> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .collect();
    assert_eq!(users.len(), 3);
    assert_eq!(users[0]["parts"][0]["text"], "slowtool");
    assert_eq!(users[1]["parts"][0]["text"], "queued first");
    assert!(
        users[2]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["filename"] == "queued.txt")
    );
    assert!(
        users[2]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["mediaType"] == "image/png")
    );
    let session = store
        .load(&crate::agent::session::SessionId::new(&id))
        .await
        .unwrap()
        .unwrap();
    let messages = session.messages();
    let tool_result = messages
        .iter()
        .position(|m| matches!(m, Message::ToolResult(_)))
        .unwrap();
    let correction = messages
        .iter()
        .position(|m| matches!(m, Message::User(u) if u.content == "queued first"))
        .unwrap();
    assert!(
        tool_result < correction,
        "Steering must arrive after the tool result"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count(),
        1,
        "Steering must update the active answer, rather than start separate runs"
    );
    for request in [&request, &attachment_request] {
        let record = store.submission(&id, request).await.unwrap().unwrap();
        assert_eq!(record.status, crate::agent::inbox::SubmissionStatus::Done);
        assert_eq!(record.run_id.as_deref(), view["run_id"].as_str());
    }
}

#[tokio::test]
async fn chat_steers_at_capacity_and_stop_cancels_steering_but_preserves_legacy_followups() {
    let f = Fixture::new().await;
    let mut ids = vec![];
    for _ in 0..4 {
        let id = f.create().await;
        assert_eq!(
            f.request(reqwest::Method::POST, "send")
                .json(&json!({"id":id,"text":"wait"}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        ids.push(id);
    }
    let id = &ids[0];
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    let legacy = crate::gateway::IncomingMessage::new(
        crate::gateway::InterfaceId("web".into()),
        id,
        "preserved followup",
    );
    store
        .submit(crate::agent::inbox::SubmissionRequest::new(
            &crate::agent::session::SessionId::new(id),
            legacy,
            crate::agent::inbox::BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    let request = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"request_id":request,"text":"cancelled clarification"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let fresh = f.create().await;
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":fresh,"text":"fifth"}))
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    for id in &ids {
        f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
            .send()
            .await
            .unwrap();
    }
    f.idle(id).await;
    assert_eq!(
        store
            .submission(id, &request)
            .await
            .unwrap()
            .unwrap()
            .status,
        crate::agent::inbox::SubmissionStatus::Cancelled
    );
    let view: Value = f
        .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(view["blocked_reason"].is_string());
    assert_eq!(view["queued"][0]["text"], "preserved followup");
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"text":"resume queue"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let history = f.idle(id).await;
    let texts: Vec<_> = history["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["parts"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, vec!["wait", "preserved followup", "resume queue"]);
}

#[tokio::test]
async fn chat_queue_limit_rejects_new_input_but_allows_retry() {
    let f = Fixture::new().await;
    let id = f.create().await;
    f.request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"wait"}))
        .send()
        .await
        .unwrap();
    let mut last = json!(null);
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    // Wait until the initial admission no longer occupies one of the queue slots.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while f.history(&id).await["messages"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for index in 0..crate::agent::inbox::MAX_QUEUED_SUBMISSIONS {
        last = json!({"id":id,"request_id":uuid::Uuid::new_v4().to_string(),"text":format!("queued {index}")});
        let mut legacy = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            &id,
            last["text"].as_str().unwrap(),
        );
        legacy.request_id = last["request_id"].as_str().unwrap().into();
        store
            .submit(crate::agent::inbox::SubmissionRequest::new(
                &crate::agent::session::SessionId::new(&id),
                legacy,
                crate::agent::inbox::BusyMode::FollowUp,
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&last)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"overflow"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "Inbox queue is full"
    );
    f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
        .send()
        .await
        .unwrap();
    f.idle(&id).await;
    assert_eq!(
        f.client
            .post(format!("{}api/chat/sessions/{id}/resume", f.base))
            .header("Cookie", &f.cookie)
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        f.request(reqwest::Method::POST, &format!("sessions/{id}/resume"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = f.history(&id).await;
            if history["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "user" && m["parts"][0]["text"] == "queued 0")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
        .send()
        .await
        .unwrap();
    f.idle(&id).await;
}

#[tokio::test]
async fn chat_unconsumed_submission_failure_releases_worker_capacity() {
    let f = Fixture::new().await;
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    for _ in 0..4 {
        let id = f.create().await;
        let mut request = crate::agent::inbox::SubmissionRequest::new(
            &crate::agent::session::SessionId::new(&id),
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("web".into()),
                &id,
                "old permissions",
            ),
            crate::agent::inbox::BusyMode::FollowUp,
        );
        request.isolated = true;
        store.submit(request).await.unwrap();
        assert_eq!(
            f.request(reqwest::Method::POST, "send")
                .json(&json!({"id":id,"text":"new followup"}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let fresh = f.create().await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let status = f
                .request(reqwest::Method::POST, "send")
                .json(&json!({"id":fresh,"text":"capacity released"}))
                .send()
                .await
                .unwrap()
                .status();
            if status == 200 {
                break;
            }
            assert_eq!(status, 429);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Unconsumed queue failure retained all worker slots");
    f.idle(&fresh).await;
}

#[tokio::test]
async fn chat_retry_preserves_legacy_admission_mode() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let request = uuid::Uuid::new_v4().to_string();
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    let mut message = crate::gateway::IncomingMessage::new(
        crate::gateway::InterfaceId("web".into()),
        &id,
        "legacy queued input",
    );
    message.request_id = request.clone();
    store
        .submit(crate::agent::inbox::SubmissionRequest::new(
            &crate::agent::session::SessionId::new(&id),
            message,
            crate::agent::inbox::BusyMode::RejectIfBusy,
        ))
        .await
        .unwrap();
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"request_id":request,"text":"legacy queued input"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(f.idle(&id).await["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn chat_rejects_invalid_requests_before_running_agent() {
    let f = Fixture::new().await;
    let id = f.create().await;
    for (payload, expected) in [
        (json!({"id":id,"text":""}), 400),
        (json!({"id":id,"text":" \n"}), 400),
        (json!({"id":id,"text":"a".repeat(32769)}), 400),
        (json!({"id":id,"text":"a".repeat(70000)}), 413),
        (json!({"id":"../private","text":"hello"}), 400),
        (json!({"id":id.to_uppercase(),"text":"hello"}), 400),
        (
            json!({"id":uuid::Uuid::new_v4().to_string(),"text":"hello"}),
            404,
        ),
        (json!({"id":id,"text":"hello","messages":[]}), 422),
    ] {
        let response = f
            .request(reqwest::Method::POST, "send")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    let denied = f
        .client
        .post(format!("{}api/chat/send", f.base))
        .header("Cookie", &f.cookie)
        .header("Origin", "https://evil.example")
        .json(&json!({"id":id,"text":"hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert_eq!(f.history(&id).await["messages"], json!([]));
    let response = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"a".repeat(8000)}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
}

#[tokio::test]
async fn chat_provider_failure_still_saves_user_message() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let response = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"fail"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(response.contains("submission_id"));
    assert_eq!(f.idle(&id).await["messages"][0]["parts"][0]["text"], "fail");
}

#[tokio::test]
#[ignore = "Local fake-model server for frontend and browser tests"]
async fn browser_fixture() {
    let root = std::env::var_os("CHAT_TEST_DATA").map(std::path::PathBuf::from);
    let listen = std::env::var("CHAT_TEST_LISTEN").unwrap_or_else(|_| "127.0.0.1:0".into());
    let f = Fixture::configured(root.as_deref(), &listen).await;
    let datasets = crate::agent::datasets::DatasetStore::open(
        root.as_deref().unwrap_or(f.data.path()).join("datasets"),
    )
    .unwrap();
    let mut fields = vec![json!({"name":"date","type":"date","required":true})];
    fields.extend(
        ["kcal", "protein", "fat", "carbs", "target"]
            .map(|name| json!({"name":name,"type":"number","required":true})),
    );
    datasets.execute(serde_json::from_value(json!({"action":"create","dataset":"demo_views","title":"Views test","date_field":"date","fields":fields})).unwrap()).unwrap();
    if datasets
        .execute(serde_json::from_value(json!({"action":"query","dataset":"demo_views"})).unwrap())
        .unwrap()["records"]
        .as_array()
        .unwrap()
        .is_empty()
    {
        let today = chrono::Utc::now().date_naive();
        let records=(-7..=1).map(|offset|json!({"id":format!("d{offset}"),"expected_revision":null,"values":[
            {"field":"date","value":(today+chrono::Duration::days(offset)).to_string()},
            {"field":"kcal","value":1900+offset*10},{"field":"protein","value":100+offset*2},
            {"field":"fat","value":60+offset},{"field":"carbs","value":200+offset*3},{"field":"target","value":2200}]})).collect::<Vec<_>>();
        datasets.execute(serde_json::from_value(json!({"action":"upsert","dataset":"demo_views","request_id":uuid::Uuid::new_v4().to_string(),"records":records})).unwrap()).unwrap();
    }
    datasets.execute(serde_json::from_value(json!({"action":"create","dataset":"demo_ranges","title":"Range test","date_field":"date","fields":[
        {"name":"date","type":"date","required":true},{"name":"low","type":"number","required":true},{"name":"high","type":"number","required":true},{"name":"target","type":"number","required":true}]})).unwrap()).unwrap();
    let range_records=[("a","2026-10-19",100,200),("b","2026-10-20",160,240),("c","2026-10-21",120,180)].map(|(id,date,low,high)|json!({"id":id,"expected_revision":null,"values":[{"field":"date","value":date},{"field":"low","value":low},{"field":"high","value":high},{"field":"target","value":200}]}));
    datasets.execute(serde_json::from_value(json!({"action":"upsert","dataset":"demo_ranges","request_id":"d529e5e0-aeb2-4de6-8772-ae3d7be2230e","records":range_records})).unwrap()).unwrap();
    datasets.execute(serde_json::from_value(json!({"action":"create","dataset":"demo_nutrition","title":"Demo nutrition","date_field":"date","fields":[
        {"name":"date","type":"date","required":true},{"name":"meal","type":"string","required":true},{"name":"kcal","type":"number","required":true}]})).unwrap()).unwrap();
    datasets.execute(serde_json::from_value(json!({"action":"upsert","dataset":"demo_nutrition","request_id":"e1e210dd-ce6a-467b-91d7-728431da55f4","records":
        ([("a","2026-10-20","Breakfast",350),("b","2026-10-20","Lunch",650),("c","2026-10-21","Dinner",400)].iter().map(|(id,date,meal,kcal)|json!({"id":id,"expected_revision":null,"values":[{"field":"date","value":date},{"field":"meal","value":meal},{"field":"kcal","value":kcal}]})).collect::<Vec<_>>())
    })).unwrap()).unwrap();
    datasets.execute(serde_json::from_value(json!({"action":"create","dataset":"demo_forms","title":"Form test","date_field":"date","fields":[
        {"name":"date","type":"date","required":true},{"name":"meal","type":"string","required":true},{"name":"kcal","type":"number","required":true},
        {"name":"extra","type":"number","required":false},{"name":"optional_date","type":"date","required":false}]})).unwrap()).unwrap();
    datasets.execute(serde_json::from_value(json!({"action":"upsert","dataset":"demo_forms","request_id":"f494ab6e-e59f-41b4-9acd-01fd5e1fd2de","records":[
        {"id":"first","expected_revision":null,"values":[{"field":"date","value":"2026-10-20"},{"field":"meal","value":"Breakfast"},{"field":"kcal","value":350},{"field":"extra","value":25},{"field":"optional_date","value":"2026-10-20"}]}]})).unwrap()).unwrap();
    println!("CHAT_TEST_URL={}", f.base);
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn chat_inbox_duplicate_and_request_status_survive_lost_response() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let request = uuid::Uuid::new_v4().to_string();
    let payload = json!({"id":id,"text":"hello","request_id":request});
    for _ in 0..2 {
        let response = f
            .request(reqwest::Method::POST, "send")
            .json(&payload)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.text().await.unwrap();
    }
    let history = f.idle(&id).await;
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
    let status = f
        .request(
            reqwest::Method::GET,
            &format!("sessions/{id}/requests/{request}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let status: Value = status.json().await.unwrap();
    assert_eq!(status["status"], "Done");
    let changed = f
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":id,"text":"changed","request_id":request}))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 409);
}

#[tokio::test]
async fn chat_watch_disconnect_is_read_only_and_stop_needs_no_observer() {
    let f = Fixture::new().await;
    let id = f.create().await;
    let request = uuid::Uuid::new_v4().to_string();
    let payload = json!({"id":id,"request_id":request,"text":"wait"});
    let response = f
        .request(reqwest::Method::POST, "send")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(accepted["request_id"], request);
    let mut observer = f
        .request(reqwest::Method::GET, &format!("sessions/{id}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(observer.status(), 200);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if String::from_utf8_lossy(&observer.chunk().await.unwrap().unwrap())
                .contains("Жду остановки")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    drop(observer);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(f.history(&id).await["busy"], true);
    let duplicate = f
        .request(reqwest::Method::POST, "send")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 200);
    let status: Value = f
        .request(
            reqwest::Method::GET,
            &format!("sessions/{id}/requests/{request}"),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"], "Running");
    for (method, path) in [
        (reqwest::Method::GET, "events"),
        (reqwest::Method::GET, "run"),
        (reqwest::Method::POST, "stop"),
        (reqwest::Method::POST, "resume"),
    ] {
        assert_eq!(
            f.client
                .request(method, format!("{}api/chat/sessions/{id}/{path}", f.base))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(f.idle(&id).await["messages"].as_array().unwrap().len(), 1);
    let view: Value = f
        .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["phase"], "Cancelled");
    assert_eq!(view["cancel_requested"], true);
}

#[tokio::test]
async fn chat_recovered_runs_share_four_worker_limit() {
    use crate::agent::{
        run_state::{Checkpoint, RunPhase},
        session::{Session, SessionId},
    };
    let data = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(data.path().join("owner-sessions")).unwrap();
    let mut ids = vec![];
    for _ in 0..5 {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = Session::new(SessionId::new(&id));
        session.push(Message::User(crate::agent::UserMessage {
            content: "wait".into(),
            files: vec![],
            images: vec![],
        }));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.isolated = Some(false);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        ids.push(id);
    }
    let scan = store.execution_lock("recovery", "scan").await.unwrap();
    let f = Fixture::configured(Some(data.path()), "127.0.0.1:0").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(scan);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let active = store
                .unfinished_runs()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.model_attempts > 0)
                .count();
            assert!(active <= 4, "Recovered workers bypassed capacity");
            if active == 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let fresh = f.create().await;
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":fresh,"text":"hello"}))
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    for id in ids {
        assert_eq!(
            f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
                .send()
                .await
                .unwrap()
                .status(),
            202
        );
    }
}

#[tokio::test]
async fn chat_stop_immediately_after_admission_is_durable() {
    let f = Fixture::new().await;
    for _ in 0..20 {
        let id = f.create().await;
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"text":"wait"}))
            .send()
            .await
            .unwrap();
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.request(reqwest::Method::POST, &format!("sessions/{id}/stop"))
                .send(),
        )
        .await
        .expect("Stop missed admission before checkpoint")
        .unwrap();
        assert_eq!(stopped.status(), 202);
        assert_eq!(f.idle(&id).await["busy"], false);
    }
}
