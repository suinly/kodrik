use super::{Tool, ToolOutput, telegram_reply::REPLY_CONTEXT};
use serde::Deserialize;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Poll,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    kind: Kind,
    id: Option<String>,
    offset: Option<usize>,
}

pub struct TelegramResultsTool;

#[async_trait::async_trait]
impl Tool for TelegramResultsTool {
    fn name(&self) -> String {
        "telegram_results".into()
    }
    fn description(&self) -> String {
        "Read saved poll results in the CURRENT Telegram chat. kind: poll. Pass id:null to list the 20 most recently updated messages and their IDs, then pass an ID for full details. Poll details include last received counts as of counts_updated_at; updated_at is the latest saved activity and may be newer. Public votes are paged in groups of 50: pass offset:null initially, then next_offset until null. total_known_votes counts observed active votes, not an authoritative total. For lists, use offset:null. Individual votes in non-anonymous polls are observed changes; individual votes may be incomplete if updates were missed. Anonymous polls never expose voters. Results persist across restarts; this reads local snapshots, not a live Telegram fetch. Available only in ordinary Telegram conversations, not guest queries. This tool does not send messages or close polls.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(Args);
        schema.insert(
            "required".into(),
            serde_json::json!(["kind", "id", "offset"]),
        );
        schema.to_value()
    }
    fn available(&self) -> bool {
        REPLY_CONTEXT
            .try_with(|context| {
                context.interface.id().0.starts_with("telegram:") && context.guest_query.is_none()
            })
            .unwrap_or(false)
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        anyhow::ensure!(
            self.available(),
            "Telegram results require an ordinary Telegram conversation"
        );
        let context = REPLY_CONTEXT.try_with(Clone::clone)?;
        let args: Args = serde_json::from_value(properties)?;
        let kind = match args.kind {
            Kind::Poll => "poll",
        };
        context
            .interface
            .read_results(
                &context.chat,
                kind,
                args.id.as_deref(),
                args.offset.unwrap_or(0),
            )
            .await
            .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Interface,
        agent::tools::telegram_reply::{REPLY_CONTEXT, ReplyContext},
        interfaces::{TelegramInterface, polls::PollStore},
    };
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn public_votes_can_be_read_page_by_page() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "test");
        store.register(42,7,json!({"id":"p1","question":"Choose?","options":[{"text":"A","voter_count":51}],"total_voter_count":51,"is_anonymous":false,"is_closed":false})).unwrap();
        for user in 1..52 {
            store.update(&json!({"update_id":user,"poll_answer":{"poll_id":"p1","user":{"id":user},"option_ids":[0]}})).unwrap();
        }
        let interface = Arc::new(
            TelegramInterface::new("test", "token", "").with_space_root(root.path().into()),
        );
        REPLY_CONTEXT
            .scope(ReplyContext::new(interface, "42".into(), None), async {
                let tool = TelegramResultsTool;
                let first = tool
                    .execute(json!({"kind":"poll","id":"p1","offset":null}))
                    .await
                    .unwrap()
                    .output;
                assert_eq!(first["votes"].as_array().unwrap().len(), 50);
                assert_eq!(first["next_offset"], 50);
                let last = tool
                    .execute(json!({"kind":"poll","id":"p1","offset":first["next_offset"]}))
                    .await
                    .unwrap()
                    .output;
                assert_eq!(last["votes"].as_array().unwrap().len(), 1);
                assert_eq!(last["next_offset"], serde_json::Value::Null);
                assert!(
                    tool.execute(json!({"kind":"checklist","id":null,"offset":1}))
                        .await
                        .is_err()
                );
                assert!(
                    tool.execute(json!({"kind":"poll","id":null,"offset":1}))
                        .await
                        .is_err()
                );
                assert!(
                    tool.execute(json!({"kind":"other","id":null,"offset":null}))
                        .await
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn poll_results_are_scoped_to_the_current_chat() {
        let root = tempfile::tempdir().unwrap();
        let interface: Arc<dyn Interface> = Arc::new(
            TelegramInterface::new("test", "token", "").with_space_root(root.path().into()),
        );
        let conversation = "42";
        let polls = PollStore::new(root.path(), "test");
        polls.register(&conversation,8,json!({"id":"p1","question":"Choose?","options":[{"text":"A","voter_count":1}],"total_voter_count":1,"is_anonymous":true,"is_closed":false})).unwrap();
        let tool = TelegramResultsTool;
        assert!(!tool.available());
        assert!(
            tool.execute(json!({"kind":"poll","id":null}))
                .await
                .is_err()
        );
        for chat in ["42", "43"] {
            let context = ReplyContext::new(interface.clone(), chat.into(), None);
            REPLY_CONTEXT
                .scope(context.clone(), async {
                    assert!(tool.available());
                    let result = tool
                        .execute(json!({"kind":"poll","id":null}))
                        .await
                        .unwrap()
                        .output;
                    assert_eq!(
                        result["polls"].as_array().unwrap().len(),
                        usize::from(chat == conversation)
                    );
                    let poll = tool.execute(json!({"kind":"poll","id":"p1"})).await;
                    if chat == conversation {
                        assert_eq!(poll.unwrap().output["poll"]["total_voter_count"], 1);
                    } else {
                        assert!(poll.is_err());
                    }
                    assert!(context.delivered().await.is_none());
                    assert!(
                        tool.execute(json!({"kind":"poll","id":"p1","chat":"42"}))
                            .await
                            .is_err()
                    );
                })
                .await;
        }
        REPLY_CONTEXT
            .scope(
                ReplyContext::new(interface, "42".into(), Some("query".into())),
                async {
                    assert!(!tool.available());
                    assert!(
                        tool.execute(json!({"kind":"poll","id":null}))
                            .await
                            .is_err()
                    );
                },
            )
            .await;
        let schema = tool.parameters();
        assert_eq!(schema["required"], json!(["kind", "id", "offset"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["id"]["type"],
            json!(["string", "null"])
        );
    }
}
