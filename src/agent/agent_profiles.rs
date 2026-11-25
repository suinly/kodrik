use std::collections::HashSet;

use anyhow::ensure;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{session::SqliteSessionStore, tools::model_selection::ModelTier};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileSettings {
    pub name: String,
    pub instructions: String,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AgentProfile {
    pub id: String,
    pub builtin: bool,
    #[serde(flatten)]
    pub settings: ProfileSettings,
}

#[derive(Serialize)]
pub(crate) struct ProfileCatalog {
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub models: Vec<String>,
    #[serde(skip)]
    pub device_tools: Vec<(String, String)>,
}

impl From<(Vec<String>, Vec<String>, Vec<String>)> for ProfileCatalog {
    fn from((tools, skills, models): (Vec<String>, Vec<String>, Vec<String>)) -> Self {
        Self {
            tools,
            skills,
            models,
            device_tools: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct InvalidProfile(pub &'static str);

impl std::fmt::Display for InvalidProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidProfile {}

impl ProfileSettings {
    pub(crate) fn validate(&self, catalog: &ProfileCatalog) -> anyhow::Result<()> {
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            InvalidProfile("Profile name must contain 1–128 bytes and no control characters")
        );
        ensure!(
            self.instructions.len() <= 4096,
            InvalidProfile("Profile instructions exceed 4096 bytes")
        );
        ensure!(
            self.model.as_ref().is_none_or(
                |model| ModelTier::from_name(model).is_some() && catalog.models.contains(model)
            ),
            InvalidProfile("Unknown or unavailable profile model role")
        );
        ensure!(
            valid_choices(&self.tools, &catalog.tools),
            InvalidProfile("Profile tools must be distinct, available choices (up to 64)")
        );
        ensure!(
            valid_choices(&self.skills, &catalog.skills),
            InvalidProfile("Profile skills must be distinct, available choices (up to 64)")
        );
        Ok(())
    }
}

fn valid_choices(values: &[String], allowed: &[String]) -> bool {
    let mut seen = HashSet::new();
    values.len() <= 64
        && values
            .iter()
            .all(|item| seen.insert(item) && allowed.contains(item))
}

const BUILTIN_PREFIX: &str = "builtin-";

fn validate_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        id.len() <= 64
            && id
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_')),
        InvalidProfile("Invalid agent profile ID")
    );
    Ok(())
}

pub(crate) fn validate_writable_id(id: &str) -> anyhow::Result<()> {
    validate_id(id)?;
    ensure!(
        !id.starts_with(BUILTIN_PREFIX),
        InvalidProfile("Bundled agent profiles cannot be changed")
    );
    Ok(())
}

fn bundled(catalog: &ProfileCatalog) -> impl Iterator<Item = AgentProfile> + '_ {
    [
        (
            "builtin-reviewer", "Reviewer",
            "Review the supplied work for correctness, regressions, safety, and missing tests. Report prioritized findings with evidence and locations. Do not change files or execute commands.",
            &["web_read", "web_search", "read_file", "file_read", "view_image"][..],
        ),
        (
            "builtin-researcher", "Researcher",
            "Research the assigned question using public sources. Read relevant pages, cite sources for important claims, and separate evidence from uncertainty. Do not publish or modify anything.",
            &["web_search", "web_read"],
        ),
        (
            "builtin-implementer", "Implementer",
            "Implement only the assigned change. Inspect relevant files, preserve unrelated edits, and run focused tests. Report the paths changed and the checks actually run. Do not commit, publish, or deploy unless explicitly authorized.",
            &["exec_command", "apply_patch", "read_file", "file_read", "view_image"],
        ),
    ].into_iter().map(move |(id, name, instructions, tools)| AgentProfile {
        id: id.into(),
        builtin: true,
        settings: ProfileSettings {
            name: name.into(),
            instructions: instructions.into(),
            model: None,
            skills: Vec::new(),
            tools: bundled_tools(tools, catalog),
        },
    })
}

fn bundled_tools(capabilities: &[&str], catalog: &ProfileCatalog) -> Vec<String> {
    let mut tools: Vec<_> = catalog
        .tools
        .iter()
        .filter(|name| {
            capabilities.contains(&name.as_str())
                || catalog.device_tools.iter().any(|(alias, capability)| {
                    alias == *name && capabilities.contains(&capability.as_str())
                })
        })
        .cloned()
        .collect();
    tools.sort_unstable();
    tools.dedup();
    tools
}

// Older saved profiles and drafts may still carry the removed execution limits.
fn stored_settings(payload: &str) -> serde_json::Result<ProfileSettings> {
    let mut value: serde_json::Value = serde_json::from_str(payload)?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("timeout_secs");
        fields.remove("max_model_calls");
    }
    serde_json::from_value(value)
}

// One store per space. This table also works for the personal runner when web is disabled.
fn init(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_profiles (
        id TEXT PRIMARY KEY NOT NULL,
        draft INTEGER NOT NULL CHECK(draft IN (0,1)),
        settings TEXT NOT NULL
    )",
    )?;
    Ok(())
}

impl SqliteSessionStore {
    pub(crate) async fn list_agent_profiles(
        &self,
        catalog: &ProfileCatalog,
    ) -> anyhow::Result<(Vec<AgentProfile>, Vec<AgentProfile>)> {
        let mut profiles: Vec<_> = bundled(catalog).collect();
        let (saved, drafts) = self
            .with_connection(|db| {
                init(db)?;
                let mut statement =
                    db.prepare("SELECT id, draft, settings FROM agent_profiles ORDER BY id")?;
                let mut rows = statement.query([])?;
                let (mut saved, mut drafts) = (Vec::new(), Vec::new());
                while let Some(row) = rows.next()? {
                    let profile = AgentProfile {
                        id: row.get(0)?,
                        builtin: false,
                        settings: stored_settings(&row.get::<_, String>(2)?)?,
                    };
                    if row.get(1)? { &mut drafts } else { &mut saved }.push(profile);
                }
                Ok((saved, drafts))
            })
            .await?;
        profiles.extend(saved);
        Ok((profiles, drafts))
    }

    pub(crate) async fn agent_profile(
        &self,
        id: &str,
        catalog: &ProfileCatalog,
    ) -> anyhow::Result<Option<AgentProfile>> {
        validate_id(id)?;
        if id.starts_with(BUILTIN_PREFIX) {
            return Ok(bundled(catalog).find(|profile| profile.id == id));
        }
        let id = id.to_owned();
        self.with_connection(move |db| {
            init(db)?;
            let payload: Option<String> = db
                .query_row(
                    "SELECT settings FROM agent_profiles WHERE id=?1 AND draft=0",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(match payload {
                Some(payload) => Some(AgentProfile {
                    id,
                    builtin: false,
                    settings: stored_settings(&payload)?,
                }),
                None => None,
            })
        })
        .await
    }

    pub(crate) async fn propose_agent_profile(
        &self,
        settings: ProfileSettings,
        catalog: &ProfileCatalog,
    ) -> anyhow::Result<AgentProfile> {
        settings.validate(catalog)?;
        let id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::to_string(&settings)?;
        self.with_connection(move |db| {
            init(db)?;
            // Generated IDs and INSERT-only semantics keep proposals from overwriting approved profiles.
            let inserted = db.execute(
                "INSERT INTO agent_profiles(id,draft,settings)
                 SELECT ?1,1,?2 WHERE (SELECT count(*) FROM agent_profiles WHERE draft=1)<64",
                params![id, payload],
            )?;
            ensure!(
                inserted == 1,
                InvalidProfile("Too many pending agent profile drafts")
            );
            Ok(AgentProfile {
                id,
                builtin: false,
                settings,
            })
        })
        .await
    }

    pub(crate) async fn save_agent_profile(
        &self,
        id: &str,
        settings: ProfileSettings,
        catalog: &ProfileCatalog,
    ) -> anyhow::Result<()> {
        validate_writable_id(id)?;
        settings.validate(catalog)?;
        let id = id.to_owned();
        let payload = serde_json::to_string(&settings)?;
        self.with_connection(move |db| {
            init(db)?;
            db.execute(
                "INSERT INTO agent_profiles(id,draft,settings) VALUES (?1,0,?2)
                         ON CONFLICT(id) DO UPDATE SET draft=0, settings=excluded.settings",
                params![id, payload],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_agent_profile(&self, id: &str) -> anyhow::Result<bool> {
        validate_writable_id(id)?;
        let id = id.to_owned();
        self.with_connection(move |db| {
            init(db)?;
            Ok(db.execute("DELETE FROM agent_profiles WHERE id=?1", [&id])? != 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ProfileCatalog {
        (
            vec!["exec_command".into(), "web_read".into()],
            vec!["reviewing".into()],
            vec!["fast".into()],
        )
            .into()
    }

    fn settings() -> ProfileSettings {
        ProfileSettings {
            name: "Review".into(),
            instructions: "Review the draft".into(),
            model: Some("fast".into()),
            skills: vec!["reviewing".into()],
            tools: vec!["web_read".into()],
        }
    }

    #[tokio::test]
    async fn profile_drafts_are_private_and_require_an_explicit_save_to_launch() {
        let dir = tempfile::tempdir().unwrap();
        let owner = SqliteSessionStore::open(dir.path().join("owner")).unwrap();
        let other = SqliteSessionStore::open(dir.path().join("other")).unwrap();
        let catalog = catalog();
        let draft = owner
            .propose_agent_profile(settings(), &catalog)
            .await
            .unwrap();
        assert!(
            owner
                .agent_profile(&draft.id, &catalog)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            owner.list_agent_profiles(&catalog).await.unwrap().1,
            vec![draft.clone()]
        );
        assert!(
            other
                .list_agent_profiles(&catalog)
                .await
                .unwrap()
                .1
                .is_empty()
        );

        let mut revised = settings();
        revised.instructions = "Reviewed by owner".into();
        owner
            .save_agent_profile(&draft.id, revised.clone(), &catalog)
            .await
            .unwrap();
        assert!(
            owner
                .list_agent_profiles(&catalog)
                .await
                .unwrap()
                .1
                .is_empty()
        );
        assert_eq!(
            owner
                .agent_profile(&draft.id, &catalog)
                .await
                .unwrap()
                .unwrap()
                .settings,
            revised
        );
        assert!(
            other
                .agent_profile(&draft.id, &catalog)
                .await
                .unwrap()
                .is_none()
        );
        drop(owner);

        let reopened = SqliteSessionStore::open(dir.path().join("owner")).unwrap();
        assert_eq!(
            reopened
                .agent_profile(&draft.id, &catalog)
                .await
                .unwrap()
                .unwrap()
                .settings
                .instructions,
            "Reviewed by owner"
        );
        assert!(reopened.delete_agent_profile(&draft.id).await.unwrap());
        assert!(
            reopened
                .agent_profile(&draft.id, &catalog)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn old_profile_limits_are_ignored_on_load() {
        let profile = settings();
        let mut legacy = serde_json::to_value(&profile).unwrap();
        legacy["timeout_secs"] = serde_json::json!(30);
        legacy["max_model_calls"] = serde_json::json!(4);
        let loaded = stored_settings(&legacy.to_string()).unwrap();
        assert_eq!(loaded, profile);
        let exported = serde_json::to_value(loaded).unwrap();
        assert!(exported.get("timeout_secs").is_none());
        assert!(exported.get("max_model_calls").is_none());
    }

    #[test]
    fn profile_settings_reject_invalid_choices() {
        let catalog = catalog();
        let valid = settings();
        assert!(valid.validate(&catalog).is_ok());
        for invalid in [
            ProfileSettings {
                instructions: "é".repeat(2049),
                ..valid.clone()
            },
            ProfileSettings {
                model: Some("smart".into()),
                ..valid.clone()
            },
            ProfileSettings {
                skills: vec!["../reviewing".into()],
                ..valid.clone()
            },
            ProfileSettings {
                tools: vec!["web_read".into(), "web_read".into()],
                ..valid
            },
        ] {
            assert!(invalid.validate(&catalog).is_err(), "{invalid:?}");
        }
        assert!(validate_id("../../example").is_err());
        assert!(validate_id("Has Capitals").is_err());
    }

    #[tokio::test]
    async fn bundled_profiles_expose_only_configured_capabilities_and_are_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("owner")).unwrap();
        let mut catalog = catalog();
        catalog
            .tools
            .extend(["device_exec".into(), "device_read".into()]);
        catalog.device_tools = vec![
            ("device_exec".into(), "exec_command".into()),
            ("device_read".into(), "read_file".into()),
            ("unavailable_device_read".into(), "read_file".into()),
        ];
        assert!(
            serde_json::to_value(&catalog)
                .unwrap()
                .get("device_tools")
                .is_none()
        );
        let (profiles, _) = store.list_agent_profiles(&catalog).await.unwrap();
        assert_eq!(profiles.len(), 3);
        for (id, tools) in [
            ("builtin-reviewer", vec!["device_read", "web_read"]),
            ("builtin-researcher", vec!["web_read"]),
            (
                "builtin-implementer",
                vec!["device_exec", "device_read", "exec_command"],
            ),
        ] {
            let profile = store.agent_profile(id, &catalog).await.unwrap().unwrap();
            assert!(profile.builtin);
            assert_eq!(profile.settings.tools, tools);
            assert!(profile.settings.validate(&catalog).is_ok());
            assert!(
                store
                    .save_agent_profile(id, settings(), &catalog)
                    .await
                    .is_err()
            );
            assert!(store.delete_agent_profile(id).await.is_err());
        }
        assert!(
            store
                .save_agent_profile("builtin-unassigned", settings(), &catalog)
                .await
                .is_err()
        );
    }
}
