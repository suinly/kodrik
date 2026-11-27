use super::{App, SessionId};
use crate::agent::session::picker::MAX_PAGE;
use std::{future::Future, time::Duration};
use tokio::task::JoinHandle;

pub(super) type Rows = Vec<(SessionId, String)>;
type Result = anyhow::Result<Rows>;

pub(super) const SEARCH_ROWS: u16 = 1;

#[derive(Default)]
pub(super) struct Pages(Option<(usize, JoinHandle<Result>)>);

impl Drop for Pages {
    fn drop(&mut self) {
        if let Some((_, task)) = &self.0 {
            task.abort();
        }
    }
}

impl Pages {
    pub(super) fn refresh<F>(&mut self, app: &mut App, load: impl FnOnce(String, usize, usize) -> F)
    where
        F: Future<Output = Result> + Send + 'static,
    {
        if (app.sessions.is_none() || !app.sessions_loading)
            && let Some((_, task)) = self.0.take()
        {
            task.abort();
        }
        let Some(rows) = &app.sessions else { return };
        if app.sessions_loading
            || app.sessions_error.is_some()
            || !app.sessions_more
            || app.session_index + 2 < rows.len()
        {
            return;
        }
        let limit = crossterm::terminal::size().map_or(1, |(_, height)| {
            usize::from(height.saturating_sub(2 + SEARCH_ROWS)).clamp(1, MAX_PAGE)
        });
        let offset = app.sessions_offset;
        let query = app.sessions_query.clone();
        let debounce = offset == 0 && !query.is_empty();
        let request = load(query, limit, offset);
        app.sessions_loading = true;
        self.0 = Some((
            limit,
            tokio::spawn(async move {
                if debounce {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                request.await
            }),
        ));
    }

    pub(super) async fn next(&mut self) -> Result {
        match &mut self.0 {
            Some((_, task)) => task.await?,
            None => std::future::pending().await,
        }
    }

    pub(super) fn receive(&mut self, app: &mut App, result: Result) {
        let Some((limit, _)) = self.0.take() else {
            return;
        };
        app.sessions_loading = false;
        let Some(rows) = app.sessions.as_mut() else {
            return;
        };
        match result {
            Ok(page) => {
                app.sessions_offset += page.len();
                app.sessions_more = page.len() == limit;
                for row in page {
                    if !rows.iter().any(|(id, _)| id == &row.0) {
                        rows.push(row);
                    }
                }
            }
            Err(error) => app.sessions_error = Some(error.to_string()),
        }
    }
}
