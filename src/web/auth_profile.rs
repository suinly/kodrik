use super::*;

#[derive(Serialize)]
pub(crate) struct Profile {
    name: String,
    pub owner: bool,
    telegram_id: Option<u64>,
    password_set: bool,
}

#[derive(Serialize)]
pub(crate) struct AccountSummary {
    #[serde(serialize_with = "serialize_telegram_id")]
    pub telegram_id: Option<u64>,
    name: String,
    pub owner: bool,
    pub blocked: bool,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum AccountChange {
    Rename { name: String },
    SetBlocked { blocked: bool },
}

fn serialize_telegram_id<S: serde::Serializer>(
    id: &Option<u64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    id.map(|id| id.to_string()).serialize(serializer)
}

#[derive(Debug)]
pub(crate) enum ProfileError {
    Unauthorized,
    Forbidden,
    Invalid(&'static str),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "Сессия истекла. Войдите снова.",
            Self::Forbidden => "Пароль может менять только владелец.",
            Self::Invalid(message) => message,
        })
    }
}

impl std::error::Error for ProfileError {}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 100 && !name.chars().any(char::is_control)
}

impl Journal {
    fn account_summary(&self, member: Option<u64>) -> Option<AccountSummary> {
        let Some(id) = member else {
            let owner = self.profile(&Identity::Owner);
            return Some(AccountSummary {
                telegram_id: owner.telegram_id,
                name: owner.name,
                owner: true,
                blocked: false,
            });
        };
        if self.owner_id == Some(id) {
            return None;
        }
        let invite = self.members.get(&id);
        let blocked = self.blocked_members.contains(&id);
        if invite.is_none() && !blocked {
            return None;
        }
        let profile = self.profile(&Identity::Telegram(id));
        Some(AccountSummary {
            telegram_id: Some(id),
            name: if profile.name.is_empty() {
                invite
                    .map(|key| self.invites[key].label.clone())
                    .unwrap_or_default()
            } else {
                profile.name
            },
            owner: false,
            blocked,
        })
    }

    fn profile_key(&self, identity: &Identity) -> String {
        match identity {
            Identity::Owner => "owner".into(),
            Identity::Telegram(user) if self.owner_id == Some(*user) => "owner".into(),
            Identity::Telegram(user) => format!("telegram:{user}"),
        }
    }

    fn profile(&self, identity: &Identity) -> Profile {
        let key = self.profile_key(identity);
        let owner = key == "owner";
        Profile {
            name: self.profiles.get(&key).cloned().unwrap_or_default(),
            owner,
            telegram_id: match identity {
                Identity::Owner => self.owner_id,
                Identity::Telegram(user) => Some(*user),
            },
            password_set: owner && self.password_hash.is_some(),
        }
    }

    pub(super) fn valid_profiles(&self) -> bool {
        self.profiles.len() <= 1025
            && self.profiles.iter().all(|(key, name)| {
                let valid_key = key == "owner"
                    || key.strip_prefix("telegram:").is_some_and(|raw| {
                        raw.parse::<u64>()
                            .is_ok_and(|user| invites::valid_user(user) && user.to_string() == raw)
                    });
                valid_key && name.trim() == name && valid_name(name)
            })
    }

    pub(super) fn replace_password(&mut self, hash: String) {
        self.password_hash = Some(hash);
        self.generation = Some(Uuid::new_v4());
        self.sessions.clear();
        self.telegram_requests.clear();
        self.devices.clear();
        self.device_pairings.clear();
    }
}

impl AuthStore {
    pub(crate) fn accounts(&self) -> anyhow::Result<Vec<AccountSummary>> {
        let journal = self.read()?;
        let mut accounts = vec![journal.account_summary(None).expect("Owner always exists")];
        let ids: BTreeSet<_> = journal
            .members
            .keys()
            .chain(&journal.blocked_members)
            .copied()
            .collect();
        accounts.extend(
            ids.into_iter()
                .filter_map(|id| journal.account_summary(Some(id))),
        );
        Ok(accounts)
    }

    pub(crate) fn account(&self, member: Option<u64>) -> anyhow::Result<Option<AccountSummary>> {
        Ok(self.read()?.account_summary(member))
    }

    pub(crate) fn update_account(
        &self,
        token: &str,
        member: Option<u64>,
        change: AccountChange,
        now: i64,
    ) -> anyhow::Result<Option<AccountSummary>> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        match journal.identity(token, now) {
            Some(Identity::Owner) => {}
            Some(Identity::Telegram(user)) if Some(user) == journal.owner_id => {}
            _ => return Err(ProfileError::Unauthorized.into()),
        }
        let Some(previous) = journal.account_summary(member) else {
            return Ok(None);
        };
        match change {
            AccountChange::Rename { name } => {
                let name = name.trim();
                if !valid_name(name) {
                    return Err(ProfileError::Invalid(
                        "Имя должно содержать от 1 до 100 символов без управляющих знаков.",
                    )
                    .into());
                }
                let identity = member.map_or(Identity::Owner, Identity::Telegram);
                journal
                    .profiles
                    .insert(journal.profile_key(&identity), name.to_owned());
                ensure!(journal.valid_profiles(), "Profile storage limit reached");
            }
            AccountChange::SetBlocked { blocked } => {
                let user =
                    member.ok_or(ProfileError::Invalid("Владельца нельзя заблокировать."))?;
                if blocked {
                    if !previous.blocked && journal.blocked_members.len() >= invites::MAX_MEMBERS {
                        return Err(ProfileError::Invalid(
                            "Достигнут лимит заблокированных аккаунтов.",
                        )
                        .into());
                    }
                    journal.blocked_members.insert(user);
                    journal.revoke_user_sessions(user);
                    journal.revoke_user_devices(user);
                } else {
                    journal.blocked_members.remove(&user);
                }
            }
        }
        self.save(&journal)?;
        Ok(journal.account_summary(member).or_else(|| {
            Some(AccountSummary {
                blocked: false,
                ..previous
            })
        }))
    }

    pub(crate) fn data_directory(&self) -> &Path {
        self.root
            .parent()
            .expect("Auth directory is under the data root")
    }

    pub(crate) fn profile(&self, token: &str, now: i64) -> anyhow::Result<Option<Profile>> {
        if !valid_token(token) {
            return Ok(None);
        }
        let journal = self.read()?;
        Ok(journal
            .identity(token, now)
            .map(|identity| journal.profile(&identity)))
    }

    pub(crate) fn update_profile(
        &self,
        token: &str,
        name: &str,
        now: i64,
    ) -> anyhow::Result<Profile> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let identity = journal
            .identity(token, now)
            .ok_or(ProfileError::Unauthorized)?;
        let name = name.trim();
        if !valid_name(name) {
            return Err(ProfileError::Invalid(
                "Имя должно содержать от 1 до 100 символов без управляющих знаков.",
            )
            .into());
        }
        journal
            .profiles
            .insert(journal.profile_key(&identity), name.into());
        self.save(&journal)?;
        Ok(journal.profile(&identity))
    }

    pub(crate) fn change_password(
        &self,
        token: &str,
        current: &str,
        new: &str,
        confirmation: &str,
    ) -> anyhow::Result<()> {
        let snapshot = {
            let _lock = self.lock()?;
            self.read()?
        };
        let identity = snapshot
            .identity(token, chrono::Utc::now().timestamp())
            .ok_or(ProfileError::Unauthorized)?;
        if !snapshot.profile(&identity).owner {
            return Err(ProfileError::Forbidden.into());
        }
        if new != confirmation {
            return Err(ProfileError::Invalid("Новый пароль и подтверждение не совпадают.").into());
        }
        validate_password(new).map_err(ProfileError::Invalid)?;
        if let Some(hash) = &snapshot.password_hash {
            if current.is_empty() || current.len() > 1024 {
                return Err(ProfileError::Invalid("Текущий пароль неверен.").into());
            }
            match Argon2::default().verify_password(current.as_bytes(), &PasswordHash::new(hash)?) {
                Ok(()) => {}
                Err(argon2::password_hash::Error::Password) => {
                    return Err(ProfileError::Invalid("Текущий пароль неверен.").into());
                }
                Err(error) => return Err(error.into()),
            }
            if current == new {
                return Err(
                    ProfileError::Invalid("Новый пароль должен отличаться от текущего.").into(),
                );
            }
        }
        let hash = hash_password(new)?;
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        // Hashing runs outside the lock; revocation and owner/password changes still win.
        if journal.generation != snapshot.generation
            || journal
                .identity(token, chrono::Utc::now().timestamp())
                .as_ref()
                != Some(&identity)
            || !journal.profile(&identity).owner
        {
            return Err(ProfileError::Unauthorized.into());
        }
        journal.replace_password(hash);
        self.save(&journal)
    }
}
