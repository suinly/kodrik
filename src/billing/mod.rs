use crate::agent::{
    llm::GenerationUsage,
    session::{Session, SqliteSessionStore},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(crate) struct AccountUsage {
    calls: i64,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    cost_usd: Option<f64>,
    partial: bool,
    tokens_partial: bool,
}

impl AccountUsage {
    fn empty() -> Self {
        Self {
            calls: 0,
            input_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            cost_usd: Some(0.0),
            partial: false,
            tokens_partial: false,
        }
    }

    fn mark_gap(&mut self) {
        self.partial = true;
        self.tokens_partial = true;
        if self.calls == 0 {
            self.cost_usd = None;
        }
    }
}

pub(crate) async fn account_usage(store: &SqliteSessionStore) -> anyhow::Result<AccountUsage> {
    let directory = store.directory().to_owned();
    tokio::task::spawn_blocking(move || read_saved_usage(&directory)).await?
}

pub(crate) fn saved_account_usage(directory: &std::path::Path) -> anyhow::Result<AccountUsage> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => anyhow::ensure!(metadata.is_dir(), "Invalid account directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AccountUsage::empty());
        }
        Err(error) => return Err(error.into()),
    }
    read_saved_usage(&directory.join("sessions"))
}

fn read_saved_usage(sessions: &std::path::Path) -> anyhow::Result<AccountUsage> {
    use rusqlite::OpenFlags;
    use std::{fs, io::ErrorKind};

    match fs::symlink_metadata(sessions) {
        Ok(metadata) => anyhow::ensure!(metadata.is_dir(), "Invalid account session directory"),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(AccountUsage::empty()),
        Err(error) => return Err(error.into()),
    }
    // SQLite's NOFOLLOW also rejects symlinks in ancestor paths (e.g. macOS /var).
    let path = fs::canonicalize(sessions)?.join(crate::agent::session::DATABASE);
    let mut usage = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => AccountUsage::empty(),
        Err(error) => return Err(error.into()),
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), "Invalid account session database");
            let db = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            db.busy_timeout(std::time::Duration::from_secs(1))?;
            let tx = db.unchecked_transaction()?;
            let usage = read_account_usage(&tx)?;
            let imported: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM schema_meta WHERE key='legacy_imported')",
                [],
                |row| row.get(0),
            )?;
            tx.commit()?;
            if imported {
                return Ok(usage);
            }
            usage
        }
    };
    for entry in fs::read_dir(sessions)? {
        if entry?.path().extension().is_some_and(|ext| ext == "json") {
            usage.mark_gap();
            break;
        }
    }
    Ok(usage)
}

fn read_account_usage(db: &Connection) -> anyhow::Result<AccountUsage> {
    let ledger: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='model_calls')",
        [],
        |row| row.get(0),
    )?;
    let mut usage = if ledger {
        // Every persisted row represents one call, including calls attributed to a child.
        db.query_row("SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_tokens), 0),
                      COALESCE(SUM(output_tokens), 0), SUM(cost_usd), COALESCE(MAX(partial), 0),
                      COALESCE(MAX(input_tokens IS NULL OR cached_tokens IS NULL OR output_tokens IS NULL), 0)
                      FROM model_calls", [], |r| Ok(AccountUsage {calls:r.get(0)?, input_tokens:r.get(1)?, cached_tokens:r.get(2)?, output_tokens:r.get(3)?, cost_usd:r.get(4)?, partial:r.get(5)?, tokens_partial:r.get(6)?}))?
    } else {
        AccountUsage::empty()
    };

    let recorded_chat_calls = if ledger {
        // Mirrors tracked_session_cost for personal stores. The child columns were added in v19.
        let child_scope: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('model_calls') WHERE name='child_id')",
            [],
            |r| r.get(0),
        )?;
        let child_count = if child_scope {
            "+ (SELECT COUNT(*) FROM model_calls WHERE child_id=s.id AND child_generation=json_extract(CAST(s.payload AS TEXT),'$.generation') AND kind='subagent' AND input_tokens IS NOT NULL)"
        } else {
            ""
        };
        format!(
            "(SELECT COUNT(*) FROM model_calls WHERE session_id=s.id AND generation=json_extract(CAST(s.payload AS TEXT),'$.generation') AND kind='chat' AND input_tokens IS NOT NULL) {child_count}"
        )
    } else {
        "0".to_owned()
    };
    // shortcut: don't parse very large transcripts for coverage; use a persisted coverage flag if precision is needed.
    let legacy_gap: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM sessions s WHERE CASE
        WHEN length(s.payload) > 2097152 OR NOT json_valid(CAST(s.payload AS TEXT)) THEN 1
        ELSE json_array_length(CAST(s.payload AS TEXT),'$.messages') > 0 AND (
            json_extract(CAST(s.payload AS TEXT),'$.legacy_cost_gap') = 1
            OR json_extract(CAST(s.payload AS TEXT),'$.token_usage') IS NULL
            OR json_extract(CAST(s.payload AS TEXT),'$.token_usage.requests') > {recorded_chat_calls}
        ) END)"
        ),
        [],
        |r| r.get(0),
    )?;
    if usage.calls == 0 {
        usage.cost_usd = Some(0.0);
    } else if usage.cost_usd.is_some_and(|cost| !cost.is_finite()) {
        usage.cost_usd = None;
        usage.partial = true;
    }
    if legacy_gap {
        usage.mark_gap();
    }
    Ok(usage)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    pub input_usd_per_million: Option<f64>,
    pub cached_input_usd_per_million: Option<f64>,
    pub output_usd_per_million: Option<f64>,
}

pub(crate) struct Call<'a> {
    pub id: &'a str,
    pub model: &'a str,
    pub kind: &'a str,
    pub run_id: Option<&'a str>,
    pub origin: Option<(&'a str, &'a str, Option<&'a str>)>,
    pub usage: GenerationUsage,
    pub rates: Option<Rates>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct SessionCost {
    pub total_usd: f64,
    pub partial: bool,
    pub calls: Vec<CallCost>,
    pub models: Vec<ModelCost>,
    #[serde(skip)]
    pub chat_calls: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelCost {
    model: String,
    total_usd: f64,
    partial: bool,
    count: i64,
}

pub(crate) fn migrate_child_origin(db: &Connection) -> anyhow::Result<()> {
    for column in ["child_id", "child_generation", "child_run_id"] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('model_calls') WHERE name=?1)",
            [column],
            |row| row.get(0),
        )?;
        if !exists {
            db.execute_batch(&format!(
                "ALTER TABLE model_calls ADD COLUMN {column} TEXT;"
            ))?;
        }
    }
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS model_calls_child ON model_calls(child_id, child_generation);
        PRAGMA user_version=19;",
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct CallCost {
    pub id: String,
    pub model: String,
    pub kind: String,
    pub cost_usd: Option<f64>,
    pub run_id: Option<String>,
}

pub(crate) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_calls (
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, generation TEXT NOT NULL,
        model TEXT NOT NULL, kind TEXT NOT NULL, run_id TEXT,
        input_tokens INTEGER, cached_tokens INTEGER, output_tokens INTEGER,
        input_rate REAL, cached_rate REAL, output_rate REAL,
        cost_usd REAL, partial INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS model_calls_session ON model_calls(session_id, generation);
    CREATE TRIGGER IF NOT EXISTS model_calls_delete_session AFTER DELETE ON sessions BEGIN
        DELETE FROM model_calls WHERE session_id=OLD.id;
    END;
    PRAGMA user_version=18;",
    )?;
    Ok(())
}

fn price(usage: GenerationUsage, rates: Rates) -> (Option<f64>, bool) {
    let input = usage
        .input_tokens
        .zip(usage.cached_tokens)
        .and_then(|(input, cached)| input.checked_sub(cached));
    let parts = [
        (input, rates.input_usd_per_million),
        (usage.cached_tokens, rates.cached_input_usd_per_million),
        (usage.output_tokens, rates.output_usd_per_million),
    ];
    let mut total = None;
    let mut partial = false;
    for (tokens, rate) in parts {
        let component = match (tokens, rate) {
            (Some(0), _) => Some(0.0),
            (Some(tokens), Some(rate)) if rate.is_finite() && rate >= 0.0 => {
                Some((tokens as f64 / 1_000_000.0) * rate)
            }
            _ => None,
        };
        if let Some(component) = component.filter(|value| value.is_finite()) {
            total = Some(total.unwrap_or(0.0) + component);
        } else {
            partial = true;
        }
    }
    if total.is_some_and(|value| !value.is_finite()) {
        return (None, true);
    }
    (total, partial)
}

pub(crate) async fn record_call(
    store: &SqliteSessionStore,
    session_id: &str,
    generation: &str,
    call: Call<'_>,
) -> anyhow::Result<()> {
    let (session, generation, id, model, kind, run_id) = (
        session_id.to_owned(),
        generation.to_owned(),
        call.id.to_owned(),
        call.model.to_owned(),
        call.kind.to_owned(),
        call.run_id.map(str::to_owned),
    );
    let usage = call.usage;
    let (child, child_generation, child_run) =
        call.origin
            .map_or((None, None, None), |(id, generation, run)| {
                (
                    Some(id.to_owned()),
                    Some(generation.to_owned()),
                    run.map(str::to_owned),
                )
            });
    let rates = call.rates.unwrap_or_default();
    let (cost, partial) = price(usage, rates);
    store.with_connection(move |db| {
        let tx = db.unchecked_transaction()?;
        let updated = tx.execute("INSERT INTO model_calls
            (id,session_id,generation,model,kind,run_id,input_tokens,cached_tokens,output_tokens,
             input_rate,cached_rate,output_rate,cost_usd,partial,child_id,child_generation,child_run_id)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
            ON CONFLICT(id) DO UPDATE SET input_tokens=excluded.input_tokens,cached_tokens=excluded.cached_tokens,
                output_tokens=excluded.output_tokens,cost_usd=excluded.cost_usd,partial=excluded.partial
            WHERE model_calls.session_id=excluded.session_id AND model_calls.generation=excluded.generation
              AND model_calls.input_tokens IS NULL AND model_calls.cached_tokens IS NULL AND model_calls.output_tokens IS NULL
              AND (excluded.input_tokens IS NOT NULL OR excluded.cached_tokens IS NOT NULL OR excluded.output_tokens IS NOT NULL)",
            params![id,session,generation,model,kind,run_id,
                usage.input_tokens.map(i64::try_from).transpose()?, usage.cached_tokens.map(i64::try_from).transpose()?, usage.output_tokens.map(i64::try_from).transpose()?,
                rates.input_usd_per_million,rates.cached_input_usd_per_million,rates.output_usd_per_million,cost,partial,child,child_generation,child_run])?;
        if updated > 0 {
            tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'", [])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

const SCOPE: &str = "((session_id=?1 AND generation=?2) OR (child_id=?1 AND child_generation=?2))";

fn read_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallCost> {
    Ok(CallCost {
        id: row.get(0)?,
        model: row.get(1)?,
        kind: row.get(2)?,
        cost_usd: row.get(3)?,
        run_id: row.get(4)?,
    })
}

#[derive(Serialize)]
pub(crate) struct ModelHistory {
    calls: Vec<CallCost>,
    next: Option<i64>,
}

pub(crate) async fn model_history(
    store: &SqliteSessionStore,
    session: &str,
    generation: &str,
    model: &str,
    before: Option<i64>,
) -> anyhow::Result<ModelHistory> {
    let (session, generation, model) =
        (session.to_owned(), generation.to_owned(), model.to_owned());
    store.with_connection(move |db| {
        let mut stmt = db.prepare(&format!("SELECT id,model,kind,cost_usd,CASE WHEN child_id=?1 THEN child_run_id ELSE run_id END,rowid FROM model_calls WHERE {SCOPE} AND model=?3 AND (?4 IS NULL OR rowid<?4) ORDER BY rowid DESC LIMIT 101"))?;
        let mut rows = stmt.query_map(params![session,generation,model,before], |r| Ok((read_call(r)?,r.get::<_,i64>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let next = (rows.len() > 100).then(|| rows[99].1);
        rows.truncate(100);
        Ok(ModelHistory {calls:rows.into_iter().map(|(call,_)| call).collect(),next})
    }).await
}

pub(crate) async fn child_cost(
    store: &SqliteSessionStore,
    parent: &str,
    generation: &str,
    child: &str,
) -> anyhow::Result<Option<RunCost>> {
    let (parent, generation, child) = (parent.to_owned(), generation.to_owned(), child.to_owned());
    store.with_connection(move |db| {
        let (count,total_usd,partial) = db.query_row("SELECT COUNT(*),COALESCE(SUM(cost_usd),0.0),COALESCE(MAX(partial),0) FROM model_calls WHERE session_id=?1 AND generation=?2 AND child_id=?3",params![parent,generation,child],|row| Ok((row.get::<_,i64>(0)?,row.get(1)?,row.get(2)?)))?;
        Ok((count > 0).then_some(RunCost {total_usd,partial}))
    }).await
}

pub(crate) async fn session_cost(
    store: &SqliteSessionStore,
    session_id: &str,
    generation: &str,
) -> anyhow::Result<SessionCost> {
    let (session, generation) = (session_id.to_owned(), generation.to_owned());
    store.with_connection(move |db| {
        let (total_usd,partial,chat_calls) = db.query_row(&format!("SELECT COALESCE(SUM(cost_usd),0.0),COALESCE(MAX(partial),0),COALESCE(SUM(((session_id=?1 AND kind='chat') OR (child_id=?1 AND kind='subagent')) AND input_tokens IS NOT NULL),0) FROM model_calls WHERE {SCOPE}"), params![session,generation], |row| Ok((row.get(0)?,row.get(1)?,row.get::<_, i64>(2)?)))?;
        let mut query = db.prepare(&format!("SELECT id,model,kind,cost_usd,CASE WHEN child_id=?1 THEN child_run_id ELSE run_id END FROM model_calls WHERE {SCOPE} ORDER BY rowid DESC LIMIT 20"))?;
        let calls = query.query_map(params![session,generation], read_call)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stmt = db.prepare(&format!("SELECT model,COALESCE(SUM(cost_usd),0.0),MAX(partial),COUNT(*) FROM model_calls WHERE {SCOPE} GROUP BY model ORDER BY model"))?;
        let models = stmt.query_map(params![session,generation], |r| Ok(ModelCost {model:r.get(0)?,total_usd:r.get(1)?,partial:r.get(2)?,count:r.get(3)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SessionCost {total_usd,partial,calls,models,chat_calls: chat_calls as u64})
    }).await
}

#[derive(Serialize)]
pub(crate) struct RunCost {
    total_usd: f64,
    partial: bool,
}

pub(crate) async fn tracked_session_cost(
    store: &SqliteSessionStore,
    session: &Session,
) -> anyhow::Result<SessionCost> {
    let mut cost = session_cost(store, session.id().as_str(), session.generation()).await?;
    // The old aggregate cannot be repriced per call.
    cost.partial |= !session.messages().is_empty()
        && (session.legacy_cost_gap() || session.token_usage().requests > cost.chat_calls);
    Ok(cost)
}

pub(crate) async fn run_cost(
    store: &SqliteSessionStore,
    session: &str,
    generation: &str,
    run: &str,
) -> anyhow::Result<Option<RunCost>> {
    let (session, generation, run) = (session.to_owned(), generation.to_owned(), run.to_owned());
    store.with_connection(move |db| {
        let (count, total_usd, partial) = db.query_row(&format!("SELECT COUNT(*),COALESCE(SUM(cost_usd),0.0),COALESCE(MAX(partial),0) FROM model_calls WHERE {SCOPE} AND CASE WHEN child_id=?1 THEN child_run_id ELSE run_id END=?3"), params![session,generation,run], |row| Ok((row.get::<_, i64>(0)?,row.get(1)?,row.get(2)?)))?;
        Ok((count > 0).then_some(RunCost {total_usd,partial}))
    }).await
}

#[cfg(test)]
mod tests;
