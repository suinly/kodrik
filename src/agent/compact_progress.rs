use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Work completed at chunk boundaries, not an estimate of provider completion time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionProgress {
    pub completed_tokens: u64,
    pub total_tokens: u64,
    pub completed_chunks: u64,
    pub total_chunks: u64,
    pub model_calls: u64,
    pub started_at: DateTime<Utc>,
    pub request_started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub background: bool,
    pub before_tokens: u64,
    pub after_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_clocks_stop_on_completion_and_clamp_clock_rollback() {
        let start = "2026-11-04T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let mut progress = CompactionProgress {
            completed_tokens: 400,
            total_tokens: 1000,
            completed_chunks: 2,
            total_chunks: 5,
            model_calls: 3,
            started_at: start,
            request_started_at: Some(start + chrono::Duration::seconds(60)),
            finished_at: None,
            background: false,
            before_tokens: 1200,
            after_tokens: None,
        };
        let text = progress.render_at(false, start + chrono::Duration::seconds(75));
        assert!(text.contains("40%"));
        assert!(text.contains("chunks 2/5"));
        assert!(text.contains("1:15"));
        assert!(text.contains("model response #3 0:15"));
        assert!(progress.render_at(true, start).contains("0:00"));
        progress.finished_at = Some(start + chrono::Duration::seconds(80));
        progress.after_tokens = Some(200);
        let text = progress.render_at(false, start + chrono::Duration::hours(1));
        assert!(text.contains("~1200 → ~200"));
        assert!(text.contains("1:20"));
        assert!(!text.contains("model response"));
        progress.total_tokens = 0;
        assert_eq!(progress.percent(), 0);
        progress.total_tokens = u64::MAX;
        progress.completed_tokens = u64::MAX;
        assert_eq!(progress.percent(), 100);
    }
}

impl CompactionProgress {
    pub fn percent(&self) -> u64 {
        if self.total_tokens == 0 {
            return 0;
        }
        ((self.completed_tokens as u128 * 100 / self.total_tokens as u128).min(100)) as u64
    }

    pub fn render(&self, russian: bool) -> String {
        self.render_at(russian, Utc::now())
    }

    pub fn render_at(&self, russian: bool, now: DateTime<Utc>) -> String {
        let end = self.finished_at.unwrap_or(now);
        let elapsed = (end - self.started_at).num_seconds().max(0);
        let label = match (russian, self.background, self.finished_at.is_some()) {
            (true, _, true) => "Краткий контекст подготовлен",
            (false, _, true) => "Context summary prepared",
            (true, true, false) => "Фоновое сжатие",
            (true, false, false) => "Ожидание сжатия",
            (false, true, false) => "Background compaction",
            (false, false, false) => "Waiting for compaction",
        };
        let mut text = format!(
            "{label} · {}% · {} {}/{} · ~{}/{} {} · {}:{:02}",
            self.percent(),
            if russian { "блоки" } else { "chunks" },
            self.completed_chunks,
            self.total_chunks,
            self.completed_tokens,
            self.total_tokens,
            if russian { "токенов" } else { "tokens" },
            elapsed / 60,
            elapsed % 60,
        );
        if let Some(after) = self.after_tokens {
            text = format!(
                "{} · ~{} → ~{} {} · {}:{:02}",
                if russian {
                    "Контекст сжат"
                } else {
                    "Context compacted"
                },
                self.before_tokens,
                after,
                if russian { "токенов" } else { "tokens" },
                elapsed / 60,
                elapsed % 60,
            );
        } else if self.finished_at.is_none()
            && let Some(started) = self.request_started_at
        {
            let seconds = (now - started).num_seconds().max(0);
            text.push_str(&format!(
                " · {} #{} {}:{:02}",
                if russian {
                    "ответ модели"
                } else {
                    "model response"
                },
                self.model_calls,
                seconds / 60,
                seconds % 60,
            ));
        }
        text
    }
}
