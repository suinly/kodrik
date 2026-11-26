use super::GenerationUsage;
use crate::{
    agent::session::{Session, SqliteSessionStore},
    billing::{Call, Rates},
};

#[derive(Clone)]
pub(crate) struct Charge {
    store: SqliteSessionStore,
    session: String,
    generation: String,
    run_id: Option<String>,
    kind: &'static str,
    origin: Option<(String, String, Option<String>)>,
}

tokio::task_local! {
    static CURRENT: Option<Charge>;
    static PARENT: Option<Charge>;
}

impl Charge {
    pub(crate) fn new(
        store: &SqliteSessionStore,
        session: &Session,
        run_id: Option<&str>,
        kind: &'static str,
    ) -> Self {
        let mut charge = PARENT
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .unwrap_or_else(|| Self {
                store: store.clone(),
                session: session.id().as_str().to_owned(),
                generation: session.generation().to_owned(),
                run_id: run_id.map(str::to_owned),
                kind,
                origin: None,
            });
        if charge.session != session.id().as_str() {
            charge.origin = Some((
                session.id().as_str().into(),
                session.generation().into(),
                run_id.map(str::to_owned),
            ));
        }
        charge.kind = if charge.kind == "subagent" && kind == "chat" {
            "subagent"
        } else {
            kind
        };
        charge
    }

    pub(crate) fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok().flatten()
    }

    pub(crate) fn as_child(mut self) -> Self {
        self.kind = "subagent";
        self
    }

    pub(crate) async fn record(
        &self,
        id: &str,
        model: &str,
        rates: Option<Rates>,
        usage: GenerationUsage,
    ) {
        if let Err(error) = crate::billing::record_call(
            &self.store,
            &self.session,
            &self.generation,
            Call {
                id,
                model,
                kind: self.kind,
                run_id: self.run_id.as_deref(),
                origin: self.origin.as_ref().map(|(id, generation, run)| {
                    (id.as_str(), generation.as_str(), run.as_deref())
                }),
                usage,
                rates,
            },
        )
        .await
        {
            log::warn!("Could not record model call cost: {error:#}");
        }
    }
}

pub(crate) async fn scope<F: std::future::Future>(charge: Option<Charge>, work: F) -> F::Output {
    CURRENT.scope(charge, work).await
}

pub(crate) async fn child_scope<F: std::future::Future>(
    charge: Option<Charge>,
    work: F,
) -> F::Output {
    PARENT.scope(charge, work).await
}
