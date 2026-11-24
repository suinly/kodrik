use crate::agent::{llm::GenerationUsage, session::SqliteSessionStore};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

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
    pub usage: GenerationUsage,
    pub rates: Option<Rates>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct SessionCost {
    pub total_usd: f64,
    pub partial: bool,
    pub calls: Vec<CallCost>,
    #[serde(skip)]
    pub chat_calls: u64,
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
    let rates = call.rates.unwrap_or_default();
    let (cost, partial) = price(usage, rates);
    store.with_connection(move |db| {
        let tx = db.unchecked_transaction()?;
        let updated = tx.execute("INSERT INTO model_calls VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
            ON CONFLICT(id) DO UPDATE SET input_tokens=excluded.input_tokens,cached_tokens=excluded.cached_tokens,
                output_tokens=excluded.output_tokens,cost_usd=excluded.cost_usd,partial=excluded.partial
            WHERE model_calls.session_id=excluded.session_id AND model_calls.generation=excluded.generation
              AND model_calls.input_tokens IS NULL AND model_calls.cached_tokens IS NULL AND model_calls.output_tokens IS NULL
              AND (excluded.input_tokens IS NOT NULL OR excluded.cached_tokens IS NOT NULL OR excluded.output_tokens IS NOT NULL)",
            params![id,session,generation,model,kind,run_id,
                usage.input_tokens.map(i64::try_from).transpose()?, usage.cached_tokens.map(i64::try_from).transpose()?, usage.output_tokens.map(i64::try_from).transpose()?,
                rates.input_usd_per_million,rates.cached_input_usd_per_million,rates.output_usd_per_million,cost,partial])?;
        if updated > 0 {
            tx.execute("UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'", [])?;
        }
        tx.commit()?;
        Ok(())
    }).await
}

pub(crate) async fn session_cost(
    store: &SqliteSessionStore,
    session_id: &str,
    generation: &str,
) -> anyhow::Result<SessionCost> {
    let (session, generation) = (session_id.to_owned(), generation.to_owned());
    store.with_connection(move |db| {
        let (total_usd,partial,chat_calls) = db.query_row("SELECT COALESCE(SUM(cost_usd),0.0),COALESCE(MAX(partial),0),COALESCE(SUM(kind='chat' AND input_tokens IS NOT NULL),0) FROM model_calls WHERE session_id=?1 AND generation=?2", params![session,generation], |row| Ok((row.get(0)?,row.get(1)?,row.get::<_, i64>(2)?)))?;
        let mut query = db.prepare("SELECT id,model,kind,cost_usd,run_id FROM model_calls WHERE session_id=?1 AND generation=?2 ORDER BY rowid DESC LIMIT 20")?;
        let calls = query.query_map(params![session,generation], |row| Ok(CallCost { id: row.get(0)?,model:row.get(1)?,kind:row.get(2)?,cost_usd:row.get(3)?,run_id:row.get(4)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SessionCost {total_usd,partial,calls,chat_calls: chat_calls as u64})
    }).await
}

#[derive(Serialize)]
pub(crate) struct RunCost {
    total_usd: f64,
    partial: bool,
}

pub(crate) async fn run_cost(
    store: &SqliteSessionStore,
    session: &str,
    generation: &str,
    run: &str,
) -> anyhow::Result<Option<RunCost>> {
    let (session, generation, run) = (session.to_owned(), generation.to_owned(), run.to_owned());
    store.with_connection(move |db| {
        let (count, total_usd, partial) = db.query_row("SELECT COUNT(*),COALESCE(SUM(cost_usd),0.0),COALESCE(MAX(partial),0) FROM model_calls WHERE session_id=?1 AND generation=?2 AND run_id=?3", params![session,generation,run], |row| Ok((row.get::<_, i64>(0)?,row.get(1)?,row.get(2)?)))?;
        Ok((count > 0).then_some(RunCost {total_usd,partial}))
    }).await
}

#[cfg(test)]
mod tests;
