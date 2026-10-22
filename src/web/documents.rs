use crate::agent::datasets::{Cell, DatasetStore, GroupBy};
use anyhow::{Context, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const MAX_DOCUMENT: usize = 512 * 1024;

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UiSource {
    pub id: String,
    pub data_json: Option<String>,
    pub query: Option<SourceQuery>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceMode {
    Query,
    Summarize,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceQuery {
    mode: SourceMode,
    dataset: String,
    date_from: Option<String>,
    date_to: Option<String>,
    last_days: Option<u16>,
    timezone: Option<String>,
    filters: Option<Vec<Cell>>,
    metrics: Option<Vec<String>>,
    group_by: Option<GroupBy>,
    limit: Option<usize>,
}

impl UiSource {
    pub(crate) fn read(&self, store: &DatasetStore, cursor: Option<&str>) -> anyhow::Result<Value> {
        self.read_at(store, cursor, chrono::Utc::now())
    }

    fn read_at(
        &self,
        store: &DatasetStore,
        cursor: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Value> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "Invalid source id"
        );
        match (&self.data_json, &self.query) {
            (Some(raw), None) => {
                ensure!(cursor.is_none(), "Inline sources have no cursor");
                ensure!(raw.len() <= 64 * 1024, "Inline data exceeds 64 KiB");
                let data: Value = serde_json::from_str(raw)?;
                let rows = data
                    .as_array()
                    .context("Inline data must be an array of flat record objects")?;
                ensure!(rows.len() <= 200, "Inline data has more than 200 rows");
                for row in rows {
                    let values = row.as_object().context("Inline rows must be objects")?;
                    ensure!(values.len() <= 32, "Inline rows have more than 32 columns");
                    for (name, value) in values {
                        ensure!(
                            !name.is_empty()
                                && name.len() <= 128
                                && !name.chars().any(char::is_control)
                                && !["__proto__", "constructor", "prototype"]
                                    .contains(&name.as_str()),
                            "Invalid inline column name"
                        );
                        ensure!(
                            value.is_null()
                                || value.is_boolean()
                                || value.is_number()
                                || value.as_str().is_some_and(|s| s.len() <= 2048),
                            "Inline values must be bounded scalars"
                        );
                    }
                }
                Ok(data)
            }
            (None, Some(query)) => {
                let mut value = serde_json::to_value(query)?;
                let object = value.as_object_mut().context("Invalid query")?;
                object.remove("mode");
                object.remove("last_days");
                object.remove("timezone");
                if let Some(days) = query.last_days {
                    ensure!((1..=366).contains(&days), "last_days must be 1–366");
                    ensure!(
                        query.date_from.is_none() && query.date_to.is_none(),
                        "last_days cannot be combined with absolute date bounds"
                    );
                    let timezone = query
                        .timezone
                        .as_deref()
                        .unwrap_or("UTC")
                        .parse::<chrono_tz::Tz>()
                        .context("Invalid source timezone")?;
                    let today = now.with_timezone(&timezone).date_naive();
                    object.insert(
                        "date_from".into(),
                        json!((today - chrono::Duration::days(i64::from(days) - 1)).to_string()),
                    );
                    object.insert(
                        "date_to".into(),
                        json!((today + chrono::Duration::days(1)).to_string()),
                    );
                } else {
                    ensure!(query.timezone.is_none(), "timezone requires last_days");
                }
                object.insert(
                    "action".into(),
                    json!(match query.mode {
                        SourceMode::Query => "query",
                        SourceMode::Summarize => "summarize",
                    }),
                );
                if let Some(cursor) = cursor {
                    object.insert("cursor".into(), json!(cursor));
                }
                store.execute(serde_json::from_value(value)?)
            }
            _ => anyhow::bail!("Each source needs exactly one of data_json or query"),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UiOperation {
    Insert,
    Update,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UiAction {
    pub id: String,
    dataset: String,
    operation: UiOperation,
    fields: Vec<String>,
    record_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FormRequest {
    request_id: uuid::Uuid,
    values: serde_json::Map<String, Value>,
    #[serde(default)]
    clear_fields: Vec<String>,
    expected_revision: Option<i64>,
}

impl UiAction {
    fn snapshot(&self, store: &DatasetStore) -> anyhow::Result<Value> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "Invalid action id"
        );
        ensure!(
            !self.fields.is_empty() && self.fields.len() <= 32,
            "Actions need 1–32 fields"
        );
        let description = store.execute(serde_json::from_value(
            json!({"action":"describe","dataset":self.dataset}),
        )?)?;
        let schema = description["definition"]["fields"]
            .as_array()
            .context("Invalid dataset schema")?;
        let mut names = std::collections::HashSet::new();
        let mut fields = Vec::new();
        for name in &self.fields {
            ensure!(names.insert(name), "Duplicate action field");
            fields.push(
                schema
                    .iter()
                    .find(|f| f["name"].as_str() == Some(name))
                    .context("Unknown action field")?
                    .clone(),
            );
        }
        let record = match self.operation {
            UiOperation::Insert => {
                ensure!(
                    self.record_id.is_none(),
                    "Insert actions cannot target a record"
                );
                for field in schema.iter().filter(|f| f["required"] == true) {
                    ensure!(
                        self.fields.iter().any(|name| field["name"] == *name),
                        "Insert action omits a required field"
                    );
                }
                Value::Null
            }
            UiOperation::Update => {
                let id = self
                    .record_id
                    .as_deref()
                    .context("Update actions require record_id")?;
                let mut record = store
                    .read_record(&self.dataset, id)?
                    .context("Action target record does not exist")?;
                record["values"]
                    .as_object_mut()
                    .context("Invalid record values")?
                    .retain(|name, _| self.fields.contains(name));
                record
            }
        };
        Ok(
            json!({"id":self.id,"dataset":self.dataset,"operation":self.operation,"fields":fields,"record_id":self.record_id,"record":record}),
        )
    }

    pub(crate) fn submit(
        &self,
        store: &DatasetStore,
        request: FormRequest,
    ) -> anyhow::Result<Value> {
        let snapshot = self.snapshot(store)?;
        ensure!(
            request.clear_fields.len() <= 32,
            "At most 32 fields may be cleared"
        );
        let mut cleared = std::collections::HashSet::new();
        for name in &request.clear_fields {
            ensure!(cleared.insert(name), "Duplicate clear field");
            ensure!(
                !request.values.contains_key(name),
                "A field cannot be supplied and cleared"
            );
            ensure!(
                snapshot["fields"]
                    .as_array()
                    .context("Invalid action fields")?
                    .iter()
                    .any(|field| field["name"] == *name && field["required"] == false),
                "Only declared optional fields may be cleared"
            );
        }
        ensure!(
            request.values.keys().all(|name| self.fields.contains(name)),
            "Only declared action fields may be written"
        );
        ensure!(
            request
                .values
                .values()
                .all(|v| v.is_string() || v.is_number() || v.is_boolean()),
            "Form values must be scalars"
        );
        let (id, mut values) = match self.operation {
            UiOperation::Insert => {
                ensure!(
                    request.clear_fields.is_empty(),
                    "Insert actions cannot clear fields"
                );
                ensure!(
                    request.expected_revision.is_none(),
                    "Insert expected_revision must be null"
                );
                (
                    format!("form_{}", request.request_id.simple()),
                    serde_json::Map::new(),
                )
            }
            UiOperation::Update => {
                ensure!(
                    request.expected_revision.is_some_and(|r| r > 0),
                    "Update requires a positive expected_revision"
                );
                let id = self.record_id.clone().context("Missing action target")?;
                let record = store
                    .read_record(&self.dataset, &id)?
                    .context("Action target record does not exist")?;
                (
                    id,
                    record["values"]
                        .as_object()
                        .context("Invalid record values")?
                        .clone(),
                )
            }
        };
        for name in request.clear_fields {
            values.remove(&name);
        }
        values.extend(request.values);
        let cells: Vec<Value> = values
            .into_iter()
            .map(|(field, value)| json!({"field":field,"value":value}))
            .collect();
        store.execute(serde_json::from_value(json!({"action":"upsert","dataset":self.dataset,"request_id":request.request_id,"records":[{"id":id,"expected_revision":request.expected_revision,"values":cells}]}))?)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    pub title: String,
    pub body: String,
    pub sources: Vec<UiSource>,
    #[serde(default)]
    pub views: Vec<super::views::UiView>,
    #[serde(default)]
    pub actions: Vec<UiAction>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishedDocument {
    pub document: Document,
    pub snapshot: Value,
}

impl Document {
    pub(crate) fn snapshot(&self, store: &DatasetStore) -> anyhow::Result<Value> {
        ensure!(
            !self.title.trim().is_empty() && self.title.chars().count() <= 200,
            "UI title must contain 1–200 characters"
        );
        ensure!(
            !self.body.trim().is_empty() && self.body.len() <= 32 * 1024,
            "UI body must contain 1–32768 bytes"
        );
        ensure!(
            self.sources.len() <= 8,
            "UI documents support at most eight sources"
        );
        let mut ids = std::collections::HashSet::new();
        let mut sources = Vec::new();
        let now = chrono::Utc::now();
        for source in &self.sources {
            ensure!(ids.insert(&source.id), "Duplicate UI source id");
            sources.push(json!({"id":source.id,"data":source.read_at(store,None,now)?}));
        }
        let views = super::views::evaluate(&self.views, &sources)?;
        ensure!(
            self.actions.len() <= 8,
            "UI documents support at most eight actions"
        );
        let mut action_ids = std::collections::HashSet::new();
        let mut actions = Vec::new();
        for action in &self.actions {
            ensure!(action_ids.insert(&action.id), "Duplicate UI action id");
            actions.push(action.snapshot(store)?);
        }
        let snapshot = json!({"version":1,"title":self.title,"body":self.body,"sources":sources,"views":views,"actions":actions});
        ensure!(
            serde_json::to_vec(&snapshot)?.len() <= MAX_DOCUMENT,
            "UI snapshot exceeds 512 KiB; narrow the source queries"
        );
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_period_uses_local_calendar_and_moves_with_the_clock() {
        let (_root, store) = store();
        store.execute(serde_json::from_value(json!({"action":"create","dataset":"days","title":"Days","date_field":"date","fields":[{"name":"date","type":"date","required":true}]})).unwrap()).unwrap();
        store.execute(serde_json::from_value(json!({"action":"upsert","dataset":"days","request_id":uuid::Uuid::new_v4().to_string(),"records":(13..=22).map(|day|json!({"id":format!("day{day}"),"expected_revision":null,"values":[{"field":"date","value":format!("2026-10-{day}")}]})).collect::<Vec<_>>()})).unwrap()).unwrap();
        let source: UiSource = serde_json::from_value(json!({"id":"week","query":{"mode":"query","dataset":"days","last_days":7,"timezone":"Europe/Moscow","limit":200}})).unwrap();
        let read = |time: &str| source.read_at(&store, None, time.parse().unwrap()).unwrap();
        let before = read("2026-10-20T20:59:59Z");
        let after = read("2026-10-20T21:00:00Z");
        assert_eq!(before["records"][0]["values"]["date"], "2026-10-14");
        assert_eq!(after["records"][0]["values"]["date"], "2026-10-15");
        assert_eq!(after["records"].as_array().unwrap().len(), 7);
        for fields in [
            json!({"last_days":0}),
            json!({"last_days":367}),
            json!({"last_days":7,"timezone":"Invalid/Zone"}),
            json!({"last_days":7,"date_from":"2026-10-01"}),
            json!({"timezone":"UTC"}),
        ] {
            let mut query = json!({"mode":"query","dataset":"days"});
            query
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let source: UiSource =
                serde_json::from_value(json!({"id":"invalid","query":query})).unwrap();
            assert!(source.read(&store, None).is_err());
        }
    }
    fn store() -> (tempfile::TempDir, DatasetStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = DatasetStore::open(dir.path().join("datasets")).unwrap();
        store.execute(serde_json::from_value(json!({"action":"create","dataset":"items","title":"Items","fields":[{"name":"name","type":"string","required":true},{"name":"note","type":"string","required":false}]})).unwrap()).unwrap();
        (dir, store)
    }
    #[test]
    fn forms_validate_resolve_and_insert_idempotently() {
        let (_dir, store) = store();
        let document: Document=serde_json::from_value(json!({"title":"Items","body":"Form","sources":[],"actions":[{"id":"add","dataset":"items","operation":"insert","fields":["name"],"record_id":null}]})).unwrap();
        let snapshot = document.snapshot(&store).unwrap();
        assert_eq!(
            snapshot["actions"][0]["fields"][0],
            json!({"name":"name","type":"string","required":true})
        );
        let action = &document.actions[0];
        let request = json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"first"},"expected_revision":null});
        let result = action
            .submit(&store, serde_json::from_value(request.clone()).unwrap())
            .unwrap();
        assert_eq!(
            action
                .submit(&store, serde_json::from_value(request.clone()).unwrap())
                .unwrap(),
            result
        );
        let mut changed = request.clone();
        changed["values"]["name"] = json!("changed");
        assert!(
            action
                .submit(&store, serde_json::from_value(changed).unwrap())
                .is_err()
        );
        let mut extra = request;
        extra["request_id"] = json!(uuid::Uuid::new_v4());
        extra["values"]["note"] = json!("forbidden");
        assert!(
            action
                .submit(&store, serde_json::from_value(extra).unwrap())
                .is_err()
        );
        assert_eq!(
            store
                .execute(
                    serde_json::from_value(json!({"action":"query","dataset":"items"})).unwrap()
                )
                .unwrap()["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let old: Document =
            serde_json::from_value(json!({"title":"Old","body":"ok","sources":[]})).unwrap();
        assert!(old.actions.is_empty());
    }
    #[test]
    fn forms_reject_invalid_declarations() {
        let (_dir, store) = store();
        let action = json!({"id":"add","dataset":"items","operation":"insert","fields":["name"],"record_id":null});
        for invalid in [
            json!({"fields":["note"]}),
            json!({"fields":["name","name"]}),
            json!({"fields":["missing"]}),
            json!({"id":"../escape"}),
            json!({"operation":"update","record_id":"missing"}),
            json!({"record_id":"one"}),
        ] {
            let mut candidate = action.clone();
            candidate
                .as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            let document: Document = serde_json::from_value(
                json!({"title":"Items","body":"Form","sources":[],"actions":[candidate]}),
            )
            .unwrap();
            assert!(document.snapshot(&store).is_err());
        }
        for actions in [vec![action.clone(), action.clone()], vec![action; 9]] {
            let document: Document = serde_json::from_value(
                json!({"title":"Items","body":"Form","sources":[],"actions":actions}),
            )
            .unwrap();
            assert!(document.snapshot(&store).is_err());
        }
    }

    #[test]
    fn forms_clear_only_declared_optional_fields() {
        let (_dir, store) = store();
        store.execute(serde_json::from_value(json!({"action":"create","dataset":"measurements","title":"Measurements","fields":[{"name":"name","type":"string","required":true},{"name":"note","type":"string","required":false},{"name":"amount","type":"number","required":false},{"name":"day","type":"date","required":false}]})).unwrap()).unwrap();
        store.execute(serde_json::from_value(json!({"action":"upsert","dataset":"measurements","request_id":uuid::Uuid::new_v4(),"records":[{"id":"one","expected_revision":null,"values":[{"field":"name","value":"keep"},{"field":"note","value":"remove"},{"field":"amount","value":5},{"field":"day","value":"2026-10-20"}]}]})).unwrap()).unwrap();
        let action:UiAction=serde_json::from_value(json!({"id":"edit","dataset":"measurements","operation":"update","fields":["name","note","amount","day"],"record_id":"one"})).unwrap();
        for (clear, values) in [
            (json!(["name"]), json!({})),
            (json!(["other"]), json!({})),
            (json!(["note", "note"]), json!({})),
            (json!(["note"]), json!({"note":"ambiguous"})),
        ] {
            assert!(action.submit(&store,serde_json::from_value(json!({"request_id":uuid::Uuid::new_v4(),"values":values,"clear_fields":clear,"expected_revision":1})).unwrap()).is_err());
        }
        let request = json!({"request_id":uuid::Uuid::new_v4(),"values":{},"clear_fields":["note","amount","day"],"expected_revision":1});
        action
            .submit(&store, serde_json::from_value(request.clone()).unwrap())
            .unwrap();
        assert_eq!(
            store.read_record("measurements", "one").unwrap().unwrap()["values"],
            json!({"name":"keep"})
        );
        let mut stale = request;
        stale["request_id"] = json!(uuid::Uuid::new_v4());
        assert!(
            action
                .submit(&store, serde_json::from_value(stale).unwrap())
                .is_err()
        );
    }

    #[test]
    fn forms_update_preserves_undeclared_values_and_rejects_stale_revisions() {
        let (_dir, store) = store();
        store.execute(serde_json::from_value(json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4(),"records":[{"id":"one","expected_revision":null,"values":[{"field":"name","value":"before"},{"field":"note","value":"keep"}]}]})).unwrap()).unwrap();
        let document:Document=serde_json::from_value(json!({"title":"Items","body":"Form","sources":[],"actions":[{"id":"edit","dataset":"items","operation":"update","fields":["name"],"record_id":"one"}]})).unwrap();
        assert_eq!(
            document.snapshot(&store).unwrap()["actions"][0]["record"]["revision"],
            1
        );
        assert_eq!(
            document.snapshot(&store).unwrap()["actions"][0]["record"]["values"],
            json!({"name":"before"})
        );
        let request = json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"after"},"expected_revision":1});
        document.actions[0]
            .submit(&store, serde_json::from_value(request).unwrap())
            .unwrap();
        assert_eq!(
            store.read_record("items", "one").unwrap().unwrap()["values"],
            json!({"name":"after","note":"keep"})
        );
        assert!(document.actions[0].submit(&store,serde_json::from_value(json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"stale"},"expected_revision":1})).unwrap()).is_err());
    }
}
