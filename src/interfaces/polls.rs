use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) struct PollStore {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    binding: Option<Binding>,
    poll: Option<Value>,
    poll_update_id: Option<u64>,
    #[serde(default)]
    counts_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    votes: BTreeMap<String, Vote>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    #[serde(deserialize_with = "deserialize_conversation")]
    chat: String,
    message_id: i32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vote {
    update_id: Option<u64>,
    answer: Value,
}

impl PollStore {
    pub(crate) fn new(root: &Path, interface: &str) -> Self {
        Self {
            root: crate::spaces::telegram_interface(root, interface).join("polls"),
        }
    }

    pub(crate) fn register(
        &self,
        chat: impl ToString,
        message_id: i32,
        poll: Value,
    ) -> anyhow::Result<()> {
        let chat = chat.to_string();
        ensure!(
            chat.parse::<i64>()
                .is_ok_and(|id| id != 0 && id.to_string() == chat),
            "Invalid poll chat ID"
        );
        ensure!(message_id > 0, "Invalid poll message");
        let id = poll_id(&poll, "id")?.to_owned();
        validate_poll(&poll)?;
        let _lock = self.lock()?;
        let mut record = self.load(&id)?;
        if let Some(binding) = &record.binding {
            ensure!(
                binding.chat == chat && binding.message_id == message_id,
                "Poll is bound to another chat or message"
            );
            return Ok(());
        }
        record.binding = Some(Binding { chat, message_id });
        if record.poll.is_none() {
            record.poll = Some(poll);
            record.counts_updated_at = Some(chrono::Utc::now());
        }
        if record
            .poll
            .as_ref()
            .is_some_and(|poll| poll["is_anonymous"] == true)
        {
            record.votes.clear();
        }
        self.save(&mut record)
    }

    pub(crate) fn update(&self, update: &Value) -> anyhow::Result<()> {
        let update_id = update["update_id"]
            .as_u64()
            .context("Missing poll update ID")?;
        if let Some(poll) = update.get("poll") {
            validate_poll(poll)?;
            let id = poll_id(poll, "id")?;
            let _lock = self.lock()?;
            let mut record = self.load(id)?;
            if record
                .poll_update_id
                .is_some_and(|previous| previous >= update_id)
            {
                return Ok(());
            }
            record.poll = Some(poll.clone());
            record.counts_updated_at = Some(chrono::Utc::now());
            record.poll_update_id = Some(update_id);
            if poll["is_anonymous"] == true {
                record.votes.clear();
            }
            self.save(&mut record)
        } else {
            let answer = update.get("poll_answer").context("Missing poll result")?;
            let id = poll_id(answer, "poll_id")?;
            let (kind, voter) = if answer.get("voter_chat").is_some() {
                ("voter_chat", &answer["voter_chat"])
            } else {
                ("user", &answer["user"])
            };
            let voter_id = voter["id"].as_i64().context("Missing poll voter ID")?;
            let options = answer["option_ids"]
                .as_array()
                .context("Missing poll vote options")?;
            ensure!(
                options.len() <= 12
                    && options
                        .iter()
                        .all(|option| option.as_u64().is_some_and(|id| id < 12)),
                "Invalid poll vote options"
            );
            let key = format!("{kind}:{voter_id}");
            let _lock = self.lock()?;
            let mut record = self.load(id)?;
            if record
                .poll
                .as_ref()
                .is_some_and(|poll| poll["is_anonymous"] == true)
            {
                return Ok(());
            }
            if record
                .votes
                .get(&key)
                .is_some_and(|vote| vote.update_id.is_some_and(|previous| previous >= update_id))
            {
                return Ok(());
            }
            // Keep retractions as tombstones so older updates cannot resurrect a vote.
            record.votes.insert(
                key,
                Vote {
                    update_id: Some(update_id),
                    answer: answer.clone(),
                },
            );
            self.save(&mut record)
        }
    }

    pub(crate) fn read(
        &self,
        chat: impl ToString,
        id: Option<&str>,
        offset: usize,
    ) -> anyhow::Result<Value> {
        let chat = chat.to_string();
        ensure!(
            id.is_some() || offset == 0,
            "Vote offset requires a poll ID"
        );
        let _lock = self.lock()?;
        if let Some(id) = id {
            validate_id(id)?;
            let record = self.load(id)?;
            let binding = record
                .binding
                .as_ref()
                .filter(|binding| binding.chat == chat)
                .context("Poll not found in this chat")?;
            let poll = record.poll.as_ref().context("Poll snapshot unavailable")?;
            let votes: Vec<_> = record
                .votes
                .values()
                .filter(|vote| {
                    poll["is_anonymous"] == false
                        && vote.answer["option_ids"]
                            .as_array()
                            .is_some_and(|options| !options.is_empty())
                })
                .map(|vote| &vote.answer)
                .collect();
            let total_known_votes = votes.len();
            let end = offset.saturating_add(50).min(total_known_votes);
            let next_offset = (end < total_known_votes).then_some(end);
            let votes: Vec<_> = votes.into_iter().skip(offset).take(50).collect();
            return Ok(
                json!({"id":record.id,"message_id":binding.message_id,"poll":poll,"votes":votes,"total_known_votes":total_known_votes,"next_offset":next_offset,"counts_updated_at":record.counts_updated_at,"updated_at":record.updated_at}),
            );
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().is_none_or(|ext| ext != "json")
            {
                continue;
            }
            let id = entry
                .path()
                .file_stem()
                .context("Invalid poll filename")?
                .to_string_lossy()
                .into_owned();
            let record = self.load(&id)?;
            if record
                .binding
                .as_ref()
                .is_some_and(|binding| binding.chat == chat)
            {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let summaries: Vec<_> = records.into_iter().take(20).map(|record| json!({"id":record.id,"message_id":record.binding.unwrap().message_id,"question":record.poll.as_ref().map(|poll| &poll["question"]),"total_voter_count":record.poll.as_ref().map(|poll| &poll["total_voter_count"]),"is_anonymous":record.poll.as_ref().map(|poll| &poll["is_anonymous"]),"is_closed":record.poll.as_ref().map(|poll| &poll["is_closed"]),"counts_updated_at":record.counts_updated_at,"updated_at":record.updated_at})).collect();
        Ok(json!({"polls":summaries}))
    }

    fn lock(&self) -> anyhow::Result<File> {
        fs::create_dir_all(&self.root)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(self.root.join("polls.lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn load(&self, id: &str) -> anyhow::Result<Record> {
        validate_id(id)?;
        match fs::read(self.root.join(format!("{id}.json"))) {
            Ok(bytes) => {
                let mut record: Record = serde_json::from_slice(&bytes)?;
                ensure!(record.id == id, "Stored poll ID mismatch");
                // Telegram may choose a new random update ID after a week without updates.
                if chrono::Utc::now() - record.updated_at >= chrono::Duration::weeks(1) {
                    record.poll_update_id = None;
                    for vote in record.votes.values_mut() {
                        vote.update_id = None;
                    }
                }
                Ok(record)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Record {
                id: id.into(),
                binding: None,
                poll: None,
                poll_update_id: None,
                counts_updated_at: None,
                votes: BTreeMap::new(),
                updated_at: chrono::Utc::now(),
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, record: &mut Record) -> anyhow::Result<()> {
        record.updated_at = chrono::Utc::now();
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(format!("{}.json", record.id)))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "Invalid poll ID"
    );
    Ok(())
}

fn deserialize_conversation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Conversation {
        Text(String),
        Id(i64),
    }
    Ok(match Conversation::deserialize(deserializer)? {
        Conversation::Text(value) => value,
        Conversation::Id(value) => value.to_string(),
    })
}

fn poll_id<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    let id = value[key].as_str().context("Missing poll ID")?;
    validate_id(id)?;
    Ok(id)
}

fn validate_poll(poll: &Value) -> anyhow::Result<()> {
    poll_id(poll, "id")?;
    ensure!(
        poll["question"]
            .as_str()
            .is_some_and(|question| !question.is_empty())
            && poll["options"].is_array()
            && poll["total_voter_count"].is_u64()
            && poll["is_anonymous"].is_boolean()
            && poll["is_closed"].is_boolean(),
        "Invalid poll snapshot"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    pub(crate) fn poll(id: &str, anonymous: bool, count: u64) -> Value {
        json!({"id":id,"question":"Choose?","options":[{"text":"A","voter_count":count},{"text":"B","voter_count":0}],"total_voter_count":count,"is_closed":false,"is_anonymous":anonymous,"type":"regular","allows_multiple_answers":false})
    }

    fn vote(id: &str, choices: &[u8]) -> Value {
        json!({"poll_id":id,"user":{"id":99,"first_name":"Ada"},"option_ids":choices})
    }

    #[test]
    fn polls_preserve_updates_before_binding_and_reopen_in_the_correct_scope() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        store
            .update(&json!({"update_id":10,"poll":poll("p1",false,1)}))
            .unwrap();
        store
            .update(&json!({"update_id":11,"poll_answer":vote("p1", &[0])}))
            .unwrap();
        assert_eq!(store.read(42, None, 0).unwrap()["polls"], json!([]));
        store.register(42, 7, poll("p1", false, 0)).unwrap();
        store.register(42, 7, poll("p1", false, 0)).unwrap();
        assert!(store.register(43, 7, poll("p1", false, 0)).is_err());
        let reopened = PollStore::new(root.path(), "main");
        let detail = reopened.read(42, Some("p1"), 0).unwrap();
        assert_eq!(detail["poll"]["total_voter_count"], 1);
        assert_eq!(detail["votes"][0]["user"]["first_name"], "Ada");
        assert_eq!(detail["message_id"], 7);
        assert!(detail["updated_at"].as_str().is_some());
        assert!(reopened.read(43, Some("p1"), 0).is_err());
        assert_eq!(reopened.read(43, None, 0).unwrap()["polls"], json!([]));
        assert!(
            PollStore::new(root.path(), "other")
                .read(42, Some("p1"), 0)
                .is_err()
        );
        assert!(reopened.read(42, Some("../outside"), 0).is_err());
        assert_eq!(reopened.read(42, None, 0).unwrap()["polls"][0]["id"], "p1");
    }

    #[test]
    fn votes_replace_and_retract_idempotently_without_overwriting_newer_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        store.register(42, 7, poll("p1", false, 0)).unwrap();
        for (id, choices) in [(10, vec![0]), (11, vec![1]), (10, vec![0])] {
            store
                .update(&json!({"update_id":id,"poll_answer":vote("p1",&choices)}))
                .unwrap();
        }
        let detail = store.read(42, Some("p1"), 0).unwrap();
        assert_eq!(detail["votes"].as_array().unwrap().len(), 1);
        assert_eq!(detail["votes"][0]["option_ids"], json!([1]));
        store
            .update(&json!({"update_id":12,"poll_answer":vote("p1",&[])}))
            .unwrap();
        store
            .update(&json!({"update_id":11,"poll_answer":vote("p1",&[1])}))
            .unwrap();
        assert_eq!(store.read(42, Some("p1"), 0).unwrap()["votes"], json!([]));
        store
            .update(&json!({"update_id":15,"poll":poll("p1",false,2)}))
            .unwrap();
        store
            .update(&json!({"update_id":14,"poll":poll("p1",false,1)}))
            .unwrap();
        assert_eq!(
            store.read(42, Some("p1"), 0).unwrap()["poll"]["total_voter_count"],
            2
        );
    }

    #[test]
    fn update_ids_can_restart_after_a_week_without_losing_existing_votes() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        store.register(42, 7, poll("p1", false, 0)).unwrap();
        store
            .update(&json!({"update_id":100,"poll":poll("p1",false,1)}))
            .unwrap();
        store
            .update(&json!({"update_id":101,"poll_answer":vote("p1",&[0])}))
            .unwrap();
        let path = crate::spaces::telegram_interface(root.path(), "main").join("polls/p1.json");
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["updated_at"] = json!(chrono::Utc::now() - chrono::Duration::days(8));
        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        store
            .update(&json!({"update_id":1,"poll":poll("p1",false,2)}))
            .unwrap();
        assert_eq!(
            store.read(42, Some("p1"), 0).unwrap()["votes"][0]["option_ids"],
            json!([0])
        );
        store
            .update(&json!({"update_id":2,"poll_answer":vote("p1",&[1])}))
            .unwrap();
        let result = store.read(42, Some("p1"), 0).unwrap();
        assert_eq!(result["poll"]["total_voter_count"], 2);
        assert_eq!(result["votes"][0]["option_ids"], json!([1]));
    }

    #[test]
    fn vote_details_are_paged_and_counts_keep_their_snapshot_time() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        store.register(42, 7, poll("p1", false, 0)).unwrap();
        let path = crate::spaces::telegram_interface(root.path(), "main").join("polls/p1.json");
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let snapshot_at = chrono::Utc::now() - chrono::Duration::hours(2);
        record["counts_updated_at"] = json!(snapshot_at);
        for user in 1..2001 {
            record["votes"][format!("user:{user}")] = json!({"update_id":user,"answer":{"poll_id":"p1","user":{"id":user,"first_name":"😀".repeat(64),"last_name":"😀".repeat(64)},"option_ids":[0]}});
        }
        std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
        store
            .update(&json!({"update_id":2001,"poll_answer":vote("p1",&[1])}))
            .unwrap();
        let page = store.read(42, Some("p1"), 0).unwrap();
        assert_eq!(page["counts_updated_at"], json!(snapshot_at));
        assert_ne!(page["updated_at"], page["counts_updated_at"]);
        assert_eq!(page["votes"].as_array().unwrap().len(), 50);
        assert_eq!(page["total_known_votes"], 2000);
        assert_eq!(page["next_offset"], 50);
        assert!(serde_json::to_vec(&page).unwrap().len() < 60_000);
        let next = store.read(42, Some("p1"), 50).unwrap();
        assert_ne!(page["votes"][0], next["votes"][0]);
        let last = store.read(42, Some("p1"), 1950).unwrap();
        assert_eq!(last["next_offset"], Value::Null);
        let mut seen = std::collections::HashSet::new();
        for offset in (0..2000).step_by(50) {
            for vote in store.read(42, Some("p1"), offset).unwrap()["votes"]
                .as_array()
                .unwrap()
            {
                assert!(seen.insert(vote["user"]["id"].as_i64().unwrap()));
            }
        }
        assert_eq!(seen.len(), 2000);
    }

    #[test]
    fn lists_are_bounded_and_concurrent_public_votes_are_not_lost() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        for index in 0..23 {
            store
                .register(42, index + 1, poll(&format!("p{index}"), false, 0))
                .unwrap();
        }
        let listed = store.read(42, None, 0).unwrap();
        assert_eq!(listed["polls"].as_array().unwrap().len(), 20);
        assert_eq!(listed["polls"][0]["id"], "p22");
        std::thread::scope(|scope| {
            for user in 1..9 {
                let root = root.path();
                scope.spawn(move || PollStore::new(root,"main").update(&json!({"update_id":user,"poll_answer":{"poll_id":"p22","user":{"id":user},"option_ids":[0]}})).unwrap());
            }
        });
        store.update(&json!({"update_id":10,"poll_answer":{"poll_id":"p22","voter_chat":{"id":-77,"title":"Channel"},"user":{"id":1},"option_ids":[1]}})).unwrap();
        let result = store.read(42, Some("p22"), 0).unwrap();
        assert_eq!(result["votes"].as_array().unwrap().len(), 9);
        assert!(
            result["votes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|vote| vote["voter_chat"]["id"] == -77)
        );
        for update in [
            json!({"poll":poll("p22",false,1)}),
            json!({"update_id":20,"poll_answer":{"poll_id":"p22","option_ids":[0]}}),
            json!({"update_id":21,"poll_answer":vote("p22",&[99])}),
        ] {
            assert!(store.update(&update).is_err());
        }
    }

    #[test]
    fn anonymous_polls_never_expose_voters_and_bad_records_are_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let store = PollStore::new(root.path(), "main");
        store
            .update(&json!({"update_id":1,"poll_answer":vote("p1",&[0])}))
            .unwrap();
        store.register(42, 7, poll("p1", true, 1)).unwrap();
        assert_eq!(store.read(42, Some("p1"), 0).unwrap()["votes"], json!([]));
        let path = crate::spaces::telegram_interface(root.path(), "main").join("polls/p1.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(
            store
                .update(&json!({"update_id":5,"poll":poll("p1",true,2)}))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        assert!(store.register(42, 8, poll("../outside", true, 0)).is_err());
    }
}
