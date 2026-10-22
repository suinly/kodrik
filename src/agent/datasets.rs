use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) mod migration;
#[cfg(test)]
mod tests;

const VERSION: i64 = 1;
const RESPONSE_LIMIT: usize = 64 * 1024;
pub(crate) const REQUEST_LIMIT: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    List,
    Describe,
    Create,
    Upsert,
    Query,
    Summarize,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldType {
    String,
    Integer,
    Number,
    Boolean,
    Date,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Field {
    name: String,
    #[serde(rename = "type")]
    kind: FieldType,
    required: bool,
}

// A closed scalar union keeps dynamic records compatible with strict tool schemas.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum Scalar {
    String(String),
    Integer(i64),
    Number(f64),
    Boolean(bool),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cell {
    field: String,
    value: Scalar,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordInput {
    id: String,
    expected_revision: Option<i64>,
    values: Vec<Cell>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GroupBy {
    Day,
    Month,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatasetRequest {
    action: Action,
    dataset: Option<String>,
    title: Option<String>,
    fields: Option<Vec<Field>>,
    date_field: Option<String>,
    request_id: Option<String>,
    pub(crate) records: Option<Vec<RecordInput>>,
    pub(crate) records_file: Option<String>,
    date_from: Option<String>,
    date_to: Option<String>,
    filters: Option<Vec<Cell>>,
    metrics: Option<Vec<String>>,
    group_by: Option<GroupBy>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Definition {
    title: String,
    fields: Vec<Field>,
    date_field: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    dataset: String,
    revision: i64,
    after: i64,
    fingerprint: String,
}

#[derive(Clone)]
pub(crate) struct DatasetStore {
    connection: Arc<Mutex<Connection>>,
    _ownership: Arc<File>,
}

impl DatasetStore {
    pub(crate) fn open(directory: PathBuf) -> anyhow::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        ensure!(
            fs::symlink_metadata(&directory)?.is_dir(),
            "Invalid dataset directory"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let ownership = super::session::ownership_lock(&directory, false)?;
        let initialization = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("init.lock"))?;
        initialization.lock()?;
        database_present(&directory)?;
        for name in ["state.sqlite3", "state.sqlite3-wal", "state.sqlite3-shm"] {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW);
            if name == "state.sqlite3" {
                options.create(true).truncate(false);
            }
            match options.open(directory.join(name)) {
                Ok(file) => {
                    ensure!(file.metadata()?.is_file(), "Invalid dataset database file");
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut db = Connection::open(directory.join("state.sqlite3"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "Unsupported dataset schema version {version}"
        );
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            let tables: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(tables == 0, "Unrecognized unversioned dataset database");
            tx.execute_batch(
                "CREATE TABLE datasets(sequence INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE,
                    definition TEXT NOT NULL, revision INTEGER NOT NULL);
                 CREATE TABLE records(sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    dataset TEXT NOT NULL REFERENCES datasets(name), id TEXT NOT NULL,
                    revision INTEGER NOT NULL, record_date TEXT, values_json TEXT NOT NULL, UNIQUE(dataset,id));
                 CREATE INDEX records_sequence ON records(dataset,sequence);
                 CREATE INDEX records_date ON records(dataset,record_date,sequence);
                 CREATE TABLE receipts(request_id TEXT PRIMARY KEY, payload TEXT NOT NULL, response TEXT NOT NULL);
                 PRAGMA user_version=1;")?;
        }
        tx.commit()?;
        db.prepare("SELECT name,definition,revision FROM datasets LIMIT 0")?;
        db.prepare(
            "SELECT dataset,id,revision,record_date,values_json,sequence FROM records LIMIT 0",
        )?;
        db.prepare("SELECT request_id,payload,response FROM receipts LIMIT 0")?;
        File::open(&directory)?.sync_all()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(db)),
            _ownership: Arc::new(ownership),
        })
    }

    pub(crate) fn read_record(&self, dataset: &str, id: &str) -> anyhow::Result<Option<Value>> {
        valid_name(dataset)?;
        ensure!(!id.is_empty() && id.len() <= 128, "Invalid record id");
        let db = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Dataset database lock poisoned"))?;
        let row: Option<(i64, String)> = db
            .query_row(
                "SELECT revision,values_json FROM records WHERE dataset=?1 AND id=?2",
                params![dataset, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(revision, raw)| {
            Ok(json!({"id":id,"revision":revision,"values":serde_json::from_str::<Value>(&raw)?}))
        })
        .transpose()
    }

    pub(crate) fn execute(&self, request: DatasetRequest) -> anyhow::Result<Value> {
        ensure!(
            serde_json::to_vec(&request)?.len() <= REQUEST_LIMIT,
            "Dataset request exceeds 256 KiB"
        );
        ensure!(
            request.records_file.is_none(),
            "records_file must be resolved by the datasets tool"
        );
        request.validate_parameters()?;
        let mut db = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Dataset database lock poisoned"))?;
        let writing = matches!(request.action, Action::Create | Action::Upsert);
        let tx = db.transaction_with_behavior(if writing {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        })?;
        let result = match request.action {
            Action::List => list(&tx, &request),
            Action::Create => create(&tx, &request),
            Action::Describe => {
                let name = request.name()?;
                let (definition, revision) = definition(&tx, name)?;
                Ok(json!({"dataset":name,"definition":definition,"revision":revision}))
            }
            Action::Upsert => upsert(&tx, &request),
            Action::Query => query(&tx, &request),
            Action::Summarize => summarize(&tx, &request),
        }?;
        tx.commit()?;
        Ok(result)
    }
}

fn database_present(directory: &Path) -> anyhow::Result<bool> {
    let present = match fs::symlink_metadata(directory.join("state.sqlite3")) {
        Ok(metadata) => {
            ensure!(metadata.is_file(), "Invalid dataset database file");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if !present {
        for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
            match fs::symlink_metadata(directory.join(name)) {
                Ok(_) => anyhow::bail!(
                    "Dataset database is missing but SQLite sidecars remain; preserve the files and restore the store before retrying"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(present)
}

impl DatasetRequest {
    fn name(&self) -> anyhow::Result<&str> {
        let name = self.dataset.as_deref().context("dataset is required")?;
        valid_name(name)?;
        Ok(name)
    }

    pub(crate) fn validate_parameters(&self) -> anyhow::Result<()> {
        let allowed: &[&str] = match self.action {
            Action::List => &["limit", "cursor"],
            Action::Describe => &["dataset"],
            Action::Create => &["dataset", "title", "fields", "date_field"],
            Action::Upsert => &["dataset", "request_id", "records", "records_file"],
            Action::Query => &[
                "dataset",
                "date_from",
                "date_to",
                "filters",
                "limit",
                "cursor",
            ],
            Action::Summarize => &[
                "dataset",
                "date_from",
                "date_to",
                "filters",
                "metrics",
                "group_by",
            ],
        };
        for (key, value) in serde_json::to_value(self)?
            .as_object()
            .context("Invalid dataset request")?
        {
            ensure!(
                key == "action" || value.is_null() || allowed.contains(&key.as_str()),
                "Parameter {key} is not supported for this action"
            );
        }
        Ok(())
    }

    fn limit(&self) -> anyhow::Result<usize> {
        let limit = self.limit.unwrap_or(50);
        ensure!((1..=200).contains(&limit), "limit must be 1–200");
        Ok(limit)
    }
}

fn valid_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "Invalid dataset or field name"
    );
    Ok(())
}

fn valid_date(date: &str) -> anyhow::Result<()> {
    ensure!(
        date.len() == 10 && date.is_ascii(),
        "Date must be YYYY-MM-DD"
    );
    let parsed =
        chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").context("Invalid calendar date")?;
    ensure!(
        parsed.format("%Y-%m-%d").to_string() == date,
        "Date must be YYYY-MM-DD"
    );
    Ok(())
}

fn definition(db: &Connection, name: &str) -> anyhow::Result<(Definition, i64)> {
    let row: Option<(String, i64)> = db
        .query_row(
            "SELECT definition,revision FROM datasets WHERE name=?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (data, revision) = row.context("Dataset does not exist; use list or create")?;
    Ok((serde_json::from_str(&data)?, revision))
}

fn create(db: &Connection, request: &DatasetRequest) -> anyhow::Result<Value> {
    let name = request.name()?;
    let mut def = Definition {
        title: request.title.clone().context("title is required")?,
        fields: request.fields.clone().context("fields is required")?,
        date_field: request.date_field.clone(),
    };
    ensure!(
        !def.title.trim().is_empty() && def.title.chars().count() <= 200,
        "title must contain 1–200 characters"
    );
    ensure!(
        (1..=32).contains(&def.fields.len()),
        "A dataset needs 1–32 fields"
    );
    def.fields.sort_by(|a, b| a.name.cmp(&b.name));
    let mut names = BTreeSet::new();
    for field in &def.fields {
        valid_name(&field.name)?;
        ensure!(names.insert(&field.name), "Duplicate field {}", field.name);
    }
    if let Some(date) = &def.date_field {
        ensure!(
            def.fields
                .iter()
                .any(|f| &f.name == date && f.kind == FieldType::Date && f.required),
            "date_field must name a required date field"
        );
    }
    db.execute("INSERT INTO datasets(name,definition,revision) VALUES (?1,?2,0) ON CONFLICT(name) DO NOTHING", params![name,serde_json::to_string(&def)?])?;
    let (existing, revision) = definition(db, name)?;
    ensure!(
        existing == def,
        "Dataset definition conflict; schemas are immutable"
    );
    Ok(json!({"dataset":name,"definition":def,"revision":revision}))
}

fn validate_value(field: &Field, scalar: &Scalar) -> anyhow::Result<Value> {
    let value = serde_json::to_value(scalar)?;
    let valid = match field.kind {
        FieldType::String => value.as_str().is_some_and(|s| s.len() <= 2048),
        FieldType::Integer => value.as_i64().is_some(),
        FieldType::Number => value.as_f64().is_some_and(f64::is_finite),
        FieldType::Boolean => value.is_boolean(),
        FieldType::Date => value.as_str().is_some_and(|s| valid_date(s).is_ok()),
    };
    ensure!(
        valid,
        "Invalid value for field {} ({:?})",
        field.name,
        field.kind
    );
    Ok(if field.kind == FieldType::Number {
        json!(value.as_f64().context("Invalid number")?)
    } else {
        value
    })
}

fn values(
    def: &Definition,
    cells: &[Cell],
    complete: bool,
) -> anyhow::Result<BTreeMap<String, Value>> {
    ensure!(cells.len() <= def.fields.len(), "Too many record fields");
    let mut result = BTreeMap::new();
    for cell in cells {
        let field = def
            .fields
            .iter()
            .find(|f| f.name == cell.field)
            .with_context(|| format!("Unknown field {}", cell.field))?;
        ensure!(
            result
                .insert(cell.field.clone(), validate_value(field, &cell.value)?)
                .is_none(),
            "Duplicate field {}",
            cell.field
        );
    }
    if complete {
        for field in def.fields.iter().filter(|f| f.required) {
            ensure!(
                result.contains_key(&field.name),
                "Required field {} is missing",
                field.name
            );
        }
        ensure!(
            serde_json::to_vec(&result)?.len() <= 8192,
            "Record exceeds 8 KiB"
        );
    }
    Ok(result)
}

fn upsert(db: &Connection, request: &DatasetRequest) -> anyhow::Result<Value> {
    let name = request.name()?;
    let request_id = uuid::Uuid::parse_str(
        request
            .request_id
            .as_deref()
            .context("request_id is required")?,
    )
    .context("request_id must be a UUID")?
    .to_string();
    let records = request.records.as_ref().context("records is required")?;
    ensure!(
        (1..=100).contains(&records.len()),
        "upsert accepts 1–100 records"
    );
    let (def, _) = definition(db, name)?;
    let mut prepared = BTreeMap::new();
    for record in records {
        ensure!(
            !record.id.is_empty()
                && record.id.len() <= 128
                && record
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "Invalid record id"
        );
        ensure!(
            record.expected_revision.is_none_or(|r| r > 0),
            "expected_revision must be null or a positive integer"
        );
        let data = values(&def, &record.values, true)?;
        ensure!(
            prepared
                .insert(record.id.clone(), (record.expected_revision, data))
                .is_none(),
            "Duplicate record id {}",
            record.id
        );
    }
    // Sorted records/fields make a reordered retry the same semantic operation.
    let payload = serde_json::to_string(&json!({"dataset":name,"records":prepared}))?;
    let receipt: Option<(String, String)> = db
        .query_row(
            "SELECT payload,response FROM receipts WHERE request_id=?",
            [&request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((original, response)) = receipt {
        ensure!(
            original == payload,
            "request_id was already used for a different payload"
        );
        return Ok(serde_json::from_str(&response)?);
    }
    let mut changed = Vec::new();
    for record in records {
        let (expected, data) = &prepared[&record.id];
        let previous: Option<i64> = db
            .query_row(
                "SELECT revision FROM records WHERE dataset=?1 AND id=?2",
                params![name, record.id],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            previous == *expected,
            "Revision conflict for record {}; query it before editing",
            record.id
        );
        let revision = previous
            .unwrap_or(0)
            .checked_add(1)
            .context("Record revision exhausted")?;
        let date = def
            .date_field
            .as_ref()
            .and_then(|f| data.get(f))
            .and_then(Value::as_str);
        db.execute("INSERT INTO records(dataset,id,revision,record_date,values_json) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(dataset,id) DO UPDATE SET revision=excluded.revision,record_date=excluded.record_date,values_json=excluded.values_json",
            params![name,record.id,revision,date,serde_json::to_string(data)?])?;
        changed.push(json!({"id":record.id,"revision":revision}));
    }
    let (_, old_revision) = definition(db, name)?;
    let revision = old_revision
        .checked_add(1)
        .context("Dataset revision exhausted")?;
    db.execute(
        "UPDATE datasets SET revision=?2 WHERE name=?1",
        params![name, revision],
    )?;
    let response = json!({"dataset":name,"revision":revision,"records":changed});
    db.execute(
        "INSERT INTO receipts VALUES (?1,?2,?3)",
        params![request_id, payload, serde_json::to_string(&response)?],
    )?;
    Ok(response)
}

fn fingerprint(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn cursor(
    request: &DatasetRequest,
    name: &str,
    revision: i64,
    fingerprint: &str,
) -> anyhow::Result<i64> {
    let Some(encoded) = &request.cursor else {
        return Ok(0);
    };
    ensure!(encoded.len() <= 1024, "Invalid dataset cursor");
    let decoded: Cursor = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded)
            .context("Invalid dataset cursor")?,
    )
    .context("Invalid dataset cursor")?;
    ensure!(
        decoded.dataset == name && decoded.fingerprint == fingerprint && decoded.after >= 0,
        "Cursor does not match this query"
    );
    ensure!(
        decoded.revision == revision,
        "Dataset changed; restart pagination without cursor"
    );
    Ok(decoded.after)
}

fn next_cursor(name: &str, revision: i64, after: i64, fingerprint: &str) -> anyhow::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
        dataset: name.into(),
        revision,
        after,
        fingerprint: fingerprint.into(),
    })?))
}

fn list(db: &Connection, request: &DatasetRequest) -> anyhow::Result<Value> {
    let limit = request.limit()?;
    let revision: i64 =
        db.query_row("SELECT coalesce(max(sequence),0) FROM datasets", [], |r| {
            r.get(0)
        })?;
    let after = cursor(request, "$list", revision, "")?;
    let mut stmt = db.prepare(
        "SELECT sequence,name,definition FROM datasets WHERE sequence>? ORDER BY sequence LIMIT ?",
    )?;
    let mut rows = stmt.query(params![after, (limit + 1) as i64])?;
    let mut datasets = Vec::new();
    let mut last = after;
    let mut more = false;
    while let Some(row) = rows.next()? {
        if datasets.len() == limit {
            more = true;
            break;
        }
        last = row.get(0)?;
        let def: Definition = serde_json::from_str(&row.get::<_, String>(2)?)?;
        datasets.push(json!({"dataset":row.get::<_,String>(1)?,"title":def.title}));
    }
    Ok(
        json!({"datasets":datasets,"next_cursor":if more { Some(next_cursor("$list",revision,last,"")?) } else { None }}),
    )
}

fn conditions(
    request: &DatasetRequest,
    def: &Definition,
) -> anyhow::Result<(String, Vec<rusqlite::types::Value>, String)> {
    let mut sql = "dataset=?".to_string();
    let mut args = vec![rusqlite::types::Value::Text(request.name()?.into())];
    if request.date_from.is_some() || request.date_to.is_some() {
        ensure!(
            def.date_field.is_some(),
            "Date filters require a designated date_field"
        );
        for (date, operator) in [(&request.date_from, ">="), (&request.date_to, "<")] {
            if let Some(date) = date {
                valid_date(date)?;
                sql.push_str(&format!(" AND record_date{operator}?"));
                args.push(date.clone().into());
            }
        }
        if let (Some(from), Some(to)) = (&request.date_from, &request.date_to) {
            ensure!(from < to, "date_from must precede date_to");
        }
    }
    let filters = request.filters.as_deref().unwrap_or_default();
    ensure!(
        filters.len() <= 8,
        "At most eight equality filters are allowed"
    );
    let filters = values(def, filters, false)?;
    for (field, value) in &filters {
        sql.push_str(" AND json_extract(values_json,?)=json_extract(?,'$')");
        args.push(format!("$.{field}").into());
        args.push(serde_json::to_string(value)?.into());
    }
    let hash = fingerprint(&(
        request.date_from.as_ref(),
        request.date_to.as_ref(),
        filters,
    ))?;
    Ok((sql, args, hash))
}

fn query(db: &Connection, request: &DatasetRequest) -> anyhow::Result<Value> {
    let name = request.name()?;
    let (def, revision) = definition(db, name)?;
    let limit = request.limit()?;
    let (condition, mut args, hash) = conditions(request, &def)?;
    let after = cursor(request, name, revision, &hash)?;
    args.push(after.into());
    args.push(((limit + 1) as i64).into());
    let mut stmt = db.prepare(&format!("SELECT sequence,id,revision,values_json FROM records WHERE {condition} AND sequence>? ORDER BY sequence LIMIT ?"))?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut records = Vec::new();
    let mut last = after;
    let mut more = false;
    let mut bytes = 0;
    while let Some(row) = rows.next()? {
        let data: Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
        let record =
            json!({"id":row.get::<_,String>(1)?,"revision":row.get::<_,i64>(2)?,"values":data});
        let size = serde_json::to_vec(&record)?.len() + 1;
        // Reserve space for metadata and the bounded continuation cursor.
        if records.len() == limit || bytes + size > RESPONSE_LIMIT - 2048 {
            more = true;
            break;
        }
        bytes += size;
        last = row.get(0)?;
        records.push(record);
    }
    Ok(json!({"dataset":name,"revision":revision,"records":records,
        "next_cursor":if more { Some(next_cursor(name,revision,last,&hash)?) } else { None }}))
}

fn summarize(db: &Connection, request: &DatasetRequest) -> anyhow::Result<Value> {
    let name = request.name()?;
    let (def, revision) = definition(db, name)?;
    let metrics = request.metrics.as_deref().unwrap_or_default();
    ensure!(
        metrics.len() <= 8,
        "At most eight numeric metrics are allowed"
    );
    let mut names = BTreeSet::new();
    let mut select = Vec::new();
    let mut metric_args = Vec::<rusqlite::types::Value>::new();
    for metric in metrics {
        ensure!(names.insert(metric), "Duplicate metric {metric}");
        ensure!(
            def.fields
                .iter()
                .any(|f| &f.name == metric
                    && matches!(f.kind, FieldType::Number | FieldType::Integer)),
            "Metric {metric} must name a numeric field"
        );
        select.push("total(json_extract(values_json,?)),avg(json_extract(values_json,?)),count(json_extract(values_json,?))");
        for _ in 0..3 {
            metric_args.push(format!("$.{metric}").into());
        }
    }
    let group = match request.group_by {
        None => "NULL",
        Some(GroupBy::Day) => "record_date",
        Some(GroupBy::Month) => "substr(record_date,1,7)",
    };
    ensure!(
        request.group_by.is_none() || def.date_field.is_some(),
        "Grouping requires a designated date_field"
    );
    let (condition, args, _) = conditions(request, &def)?;
    metric_args.extend(args);
    let fields = if select.is_empty() {
        String::new()
    } else {
        format!(",{}", select.join(","))
    };
    let grouping = if request.group_by.is_some() {
        " GROUP BY 1 ORDER BY 1"
    } else {
        ""
    };
    let mut stmt = db.prepare(&format!(
        "SELECT {group},count(*){fields} FROM records WHERE {condition}{grouping} LIMIT 367"
    ))?;
    let mut rows = stmt.query(rusqlite::params_from_iter(metric_args))?;
    let mut groups = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(
            groups.len() < 366,
            "Too many summary groups; narrow the date range or group by month"
        );
        let mut totals = serde_json::Map::new();
        for (index, metric) in metrics.iter().enumerate() {
            let sum: f64 = row.get(2 + index * 3)?;
            let average: Option<f64> = row.get(3 + index * 3)?;
            ensure!(
                sum.is_finite() && average.is_none_or(f64::is_finite),
                "Nonfinite aggregate for metric {metric}"
            );
            totals.insert(
                metric.clone(),
                json!({"sum":sum,"average":average,"count":row.get::<_,i64>(4+index*3)?}),
            );
        }
        groups.push(json!({"group":row.get::<_,Option<String>>(0)?,"count":row.get::<_,i64>(1)?,"metrics":totals}));
    }
    Ok(json!({"dataset":name,"revision":revision,"groups":groups}))
}
