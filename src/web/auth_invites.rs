use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

const MAX_INVITES: usize = 1024;
const MAX_MEMBERS: usize = 1024;
const MAX_INVITE_SECONDS: i64 = 30 * 86400;

pub(super) fn valid_invite_token(token: &str) -> bool {
    valid_token(token)
        || token.len() == 22
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredInvite {
    label: String,
    expires: i64,
    pub(super) user_id: Option<u64>,
    revoked: bool,
}

#[derive(Debug)]
pub(crate) struct InvalidInvite;

impl std::fmt::Display for InvalidInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invitation is invalid, expired, or already used")
    }
}

impl std::error::Error for InvalidInvite {}

pub(crate) struct Invite {
    pub id: String,
    pub token: String,
    pub expires: i64,
}

pub(super) fn valid_user(user: u64) -> bool {
    user > 0 && user <= i64::MAX as u64
}

impl Journal {
    pub(super) fn allowed_identity(&self, identity: &Identity) -> bool {
        match identity {
            Identity::Owner => true,
            Identity::Telegram(user) => self.allowed_user(*user),
        }
    }

    pub(super) fn allowed_user(&self, user: u64) -> bool {
        valid_user(user) && (self.owner_id == Some(user) || self.members.contains_key(&user))
    }

    pub(super) fn valid_membership(&self) -> bool {
        self.owner_id.is_none_or(valid_user)
            && self.invites.len() <= MAX_INVITES
            && self.members.len() <= MAX_MEMBERS
            && self.invites.iter().all(|(id, invite)| {
                valid_token(id)
                    && !invite.label.trim().is_empty()
                    && invite.label.chars().count() <= 128
                    && invite.expires > 0
                    && invite.user_id.is_none_or(valid_user)
            })
            && self.members.iter().all(|(user, id)| {
                valid_user(*user)
                    && self
                        .invites
                        .get(id)
                        .is_some_and(|invite| !invite.revoked && invite.user_id == Some(*user))
            })
    }

    pub(super) fn usable_invite(&self, hash: &str, now: i64) -> bool {
        self.invites.get(hash).is_some_and(|invite| {
            !invite.revoked && invite.user_id.is_none() && invite.expires > now
        })
    }

    pub(super) fn revoke_user_sessions(&mut self, user: u64) {
        self.sessions
            .retain(|_, session| session.grant().identity != Identity::Telegram(user));
        self.telegram_requests
            .retain(|_, request| request.user_id != Some(user));
    }
}

impl AuthStore {
    pub(crate) fn configure_owner(&self, owner_id: Option<u64>) -> anyhow::Result<()> {
        ensure!(owner_id.is_none_or(valid_user), "Invalid Telegram owner ID");
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        if journal.owner_id != owner_id {
            if let Some(previous) = journal.owner_id {
                journal.revoke_user_sessions(previous);
            }
            journal.owner_id = owner_id;
            self.save(&journal)?;
        }
        Ok(())
    }

    pub(crate) fn allowed_user(&self, user: u64) -> anyhow::Result<bool> {
        Ok(self.read()?.allowed_user(user))
    }

    pub(crate) fn create_invite(
        &self,
        label: &str,
        expires: i64,
        now: i64,
    ) -> anyhow::Result<Invite> {
        let label = label.trim();
        ensure!(
            !label.is_empty() && label.chars().count() <= 128,
            "Invite label must contain 1–128 characters"
        );
        ensure!(
            now >= 0 && expires > now && expires.saturating_sub(now) <= MAX_INVITE_SECONDS,
            "Invite expiry must be within 30 days"
        );
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        ensure!(journal.invites.len() < MAX_INVITES, "Too many invites");
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let token = URL_SAFE_NO_PAD.encode(random);
        let id = token_hash(&token);
        journal.invites.insert(
            id.clone(),
            StoredInvite {
                label: label.into(),
                expires,
                user_id: None,
                revoked: false,
            },
        );
        self.save(&journal)?;
        Ok(Invite { id, token, expires })
    }

    pub(crate) fn list_invites(&self) -> anyhow::Result<serde_json::Value> {
        let journal = self.read()?;
        let invites: Vec<_> = journal
            .invites
            .iter()
            .map(|(id, invite)| {
                serde_json::json!({"id":id,"label":invite.label,"expires":invite.expires,
                "user_id":invite.user_id,"revoked":invite.revoked})
            })
            .collect();
        let users: Vec<_> = journal.members.iter().map(|(user, id)| {
            serde_json::json!({"user_id":user,"invite_id":id,"label":journal.invites[id].label})
        }).collect();
        Ok(serde_json::json!({"invites":invites,"users":users}))
    }

    pub(crate) fn revoke_invite(&self, id: &str) -> anyhow::Result<bool> {
        ensure!(valid_token(id), "Invalid invite ID");
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let Some(invite) = journal.invites.get_mut(id) else {
            return Ok(false);
        };
        invite.revoked = true;
        if let Some(user) = invite.user_id {
            if journal.members.get(&user).is_some_and(|grant| grant == id) {
                journal.members.remove(&user);
                journal.revoke_user_sessions(user);
            }
        }
        self.save(&journal)?;
        Ok(true)
    }

    pub(crate) fn revoke_user(&self, user: u64) -> anyhow::Result<bool> {
        ensure!(valid_user(user), "Invalid Telegram user ID");
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        ensure!(
            journal.owner_id != Some(user),
            "Configured owner cannot be revoked"
        );
        let grant = journal.members.remove(&user);
        if let Some(id) = &grant {
            journal.invites.get_mut(id).unwrap().revoked = true;
        }
        let removed = grant.is_some();
        journal.revoke_user_sessions(user);
        self.save(&journal)?;
        Ok(removed)
    }

    #[cfg(test)]
    pub(crate) fn grant_test_user(&self, user: u64) -> anyhow::Result<()> {
        ensure!(valid_user(user), "Invalid Telegram user ID");
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let id = token_hash(&format!("test-user-{user}"));
        journal.invites.insert(
            id.clone(),
            StoredInvite {
                label: "Test user".into(),
                expires: i64::MAX,
                user_id: Some(user),
                revoked: false,
            },
        );
        journal.members.insert(user, id);
        self.save(&journal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approve(store: &AuthStore, invite: Option<&str>, user: u64) -> telegram::BrowserLogin {
        let login = store
            .begin_telegram_login_with_invite(None, "/chat/", invite, 100)
            .unwrap()
            .unwrap();
        assert!(
            store
                .present_telegram_login(&login.id, "main", user, 101)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .decide_telegram_login(&login.id, "main", user, true, 102)
                .unwrap()
        );
        login
    }

    fn setup() -> (tempfile::TempDir, AuthStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.register_telegram_bot("main", "TestBot").unwrap();
        (dir, store)
    }

    #[test]
    fn previously_issued_long_invitation_codes_still_redeem() {
        let (_dir, store) = setup();
        let invite = store.create_invite("Legacy", 500, 100).unwrap();
        let legacy = "ab".repeat(32);
        {
            let _lock = store.lock().unwrap();
            let mut journal = store.read().unwrap();
            let invite = journal.invites.remove(&invite.id).unwrap();
            journal.invites.insert(token_hash(&legacy), invite);
            store.save(&journal).unwrap();
        }
        let login = approve(&store, Some(&legacy), 7);
        assert!(matches!(
            store.poll_telegram_login(&login.cookie, 103).unwrap(),
            LoginPoll::Approved { .. }
        ));
        assert!(store.allowed_user(7).unwrap());
    }

    #[test]
    fn absent_and_legacy_memberships_are_closed_but_password_owner_survives() {
        let (dir, store) = setup();
        assert!(!store.allowed_user(42).unwrap());
        assert_eq!(
            store.test_session(Identity::Telegram(42), 100).unwrap(),
            None
        );
        let login = store.begin_telegram_login(None, "/", 100).unwrap().unwrap();
        assert!(
            store
                .present_telegram_login(&login.id, "main", 42, 101)
                .unwrap()
                .is_none()
        );
        let token = "a".repeat(64);
        let owner = "b".repeat(64);
        let journal = serde_json::json!({"password_hash":null,"generation":null,
            "sessions":{token_hash(&token):{"identity":{"Telegram":42},"expires":1000},
                token_hash(&owner):1000}});
        fs::write(
            dir.path().join("web/auth.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert_eq!(store.identity(&token, 101).unwrap(), None);
        assert_eq!(store.identity(&owner, 101).unwrap(), Some(Identity::Owner));
        assert!(!store.allowed_user(42).unwrap());
    }

    #[test]
    fn invitation_requires_browser_secret_and_grants_only_after_approval() {
        let (dir, store) = setup();
        let invite = store
            .create_invite(" Alice \"<script>\" ", 500, 100)
            .unwrap();
        assert_eq!(invite.token.len(), 22);
        assert!(
            invite
                .token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        );
        assert_eq!(invite.expires, 500);
        let login = store
            .begin_telegram_login_with_invite(None, "/chat/", Some(&invite.token), 100)
            .unwrap()
            .unwrap();
        assert!(!store.allowed_user(42).unwrap());
        store
            .present_telegram_login(&login.id, "main", 42, 101)
            .unwrap();
        assert_eq!(
            store.poll_telegram_login(&login.cookie, 102).unwrap(),
            LoginPoll::Pending
        );
        store
            .decide_telegram_login(&login.id, "main", 42, true, 102)
            .unwrap();
        assert_eq!(
            store
                .poll_telegram_login(&format!("{}.{}", login.id, "c".repeat(64)), 103)
                .unwrap(),
            LoginPoll::Expired
        );
        assert!(!store.allowed_user(42).unwrap());
        let LoginPoll::Approved { token, return_to } =
            store.poll_telegram_login(&login.cookie, 103).unwrap()
        else {
            panic!("not approved");
        };
        assert_eq!(return_to, "/chat/");
        assert!(store.allowed_user(42).unwrap());
        assert_eq!(
            store.identity(&token, 103).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert!(!store.authenticated(&token, 103).unwrap());
        assert_eq!(
            store.poll_telegram_login(&login.cookie, 103).unwrap(),
            LoginPoll::Expired
        );
        assert!(
            store
                .begin_telegram_login_with_invite(None, "/", Some(&invite.token), 104)
                .err()
                .unwrap()
                .is::<InvalidInvite>()
        );
        let saved = fs::read_to_string(dir.path().join("web/auth.json")).unwrap();
        assert!(!saved.contains(&invite.token));
        assert!(!saved.contains(&token));
        assert_eq!(
            store.list_invites().unwrap()["users"][0]["label"],
            "Alice \"<script>\""
        );
        store.set_password("new password").unwrap();
        assert!(store.allowed_user(42).unwrap());
    }

    #[test]
    fn concurrent_browsers_cannot_grant_one_invite_to_two_users() {
        let (_dir, store) = setup();
        let invite = store.create_invite("Friend", 500, 100).unwrap();
        let first = approve(&store, Some(&invite.token), 42);
        let second = approve(&store, Some(&invite.token), 43);
        std::thread::scope(|scope| {
            let a = scope.spawn(|| store.poll_telegram_login(&first.cookie, 103).unwrap());
            let b = scope.spawn(|| store.poll_telegram_login(&second.cookie, 103).unwrap());
            let results = [a.join().unwrap(), b.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(r, LoginPoll::Approved { .. }))
                    .count(),
                1
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(r, LoginPoll::Denied))
                    .count(),
                1
            );
        });
        assert_ne!(
            store.allowed_user(42).unwrap(),
            store.allowed_user(43).unwrap()
        );
        assert_eq!(
            store.list_invites().unwrap()["users"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn failed_session_creation_does_not_consume_the_invite() {
        let (_dir, store) = setup();
        let mut tokens = Vec::new();
        for user in 1..=64 {
            store.grant_test_user(user).unwrap();
            tokens.push(
                store
                    .test_session(Identity::Telegram(user), 100)
                    .unwrap()
                    .unwrap(),
            );
        }
        let invite = store.create_invite("Next friend", 500, 100).unwrap();
        let login = approve(&store, Some(&invite.token), 65);
        assert!(store.poll_telegram_login(&login.cookie, 103).is_err());
        assert!(!store.allowed_user(65).unwrap());
        assert!(store.read().unwrap().usable_invite(&invite.id, 103));
        store.logout(&tokens[0]).unwrap();
        assert!(matches!(
            store.poll_telegram_login(&login.cookie, 104).unwrap(),
            LoginPoll::Approved { .. }
        ));
        assert!(store.allowed_user(65).unwrap());
    }

    #[test]
    fn existing_member_consumes_a_new_invite_and_replaces_the_grant() {
        let (_dir, store) = setup();
        let first = store.create_invite("Friend", 500, 100).unwrap();
        let login = approve(&store, Some(&first.token), 42);
        assert!(matches!(
            store.poll_telegram_login(&login.cookie, 103).unwrap(),
            LoginPoll::Approved { .. }
        ));
        let second = store.create_invite("Friend again", 500, 100).unwrap();
        let login = approve(&store, Some(&second.token), 42);
        assert!(matches!(
            store.poll_telegram_login(&login.cookie, 103).unwrap(),
            LoginPoll::Approved { .. }
        ));
        assert!(
            store
                .begin_telegram_login_with_invite(None, "/", Some(&second.token), 104)
                .err()
                .unwrap()
                .is::<InvalidInvite>()
        );
        assert_eq!(store.read().unwrap().members[&42], second.id);
        store.revoke_invite(&first.id).unwrap();
        assert!(store.allowed_user(42).unwrap());
        store.revoke_invite(&second.id).unwrap();
        assert!(!store.allowed_user(42).unwrap());
    }

    #[test]
    fn invite_expiry_and_revocation_are_rechecked_at_redemption() {
        let (_dir, store) = setup();
        for revoked in [false, true] {
            let invite = store.create_invite("Friend", 104, 100).unwrap();
            let login = approve(&store, Some(&invite.token), 42);
            if revoked {
                assert!(store.revoke_invite(&invite.id).unwrap());
            }
            assert_eq!(
                store
                    .poll_telegram_login(&login.cookie, if revoked { 103 } else { 104 })
                    .unwrap(),
                LoginPoll::Denied
            );
            assert!(!store.allowed_user(42).unwrap());
            assert!(
                store
                    .begin_telegram_login_with_invite(None, "/", Some(&invite.token), 104)
                    .err()
                    .unwrap()
                    .is::<InvalidInvite>()
            );
        }
    }

    #[test]
    fn revocation_invalidates_sessions_and_pending_logins_without_resurrection() {
        let (_dir, store) = setup();
        let invite = store.create_invite("Friend", 500, 100).unwrap();
        let login = approve(&store, Some(&invite.token), 42);
        let LoginPoll::Approved { token, .. } =
            store.poll_telegram_login(&login.cookie, 103).unwrap()
        else {
            panic!("not approved");
        };
        let pending = approve(&store, None, 42);
        assert!(store.revoke_invite(&invite.id).unwrap());
        assert_eq!(store.identity(&token, 104).unwrap(), None);
        assert_eq!(
            store.test_session(Identity::Telegram(42), 104).unwrap(),
            None
        );
        assert_eq!(
            store.poll_telegram_login(&pending.cookie, 104).unwrap(),
            LoginPoll::Expired
        );
        let replacement = store.create_invite("Friend again", 500, 100).unwrap();
        let replacement_login = approve(&store, Some(&replacement.token), 42);
        assert!(matches!(
            store
                .poll_telegram_login(&replacement_login.cookie, 104)
                .unwrap(),
            LoginPoll::Approved { .. }
        ));
        assert!(store.revoke_invite(&invite.id).unwrap());
        assert!(
            store.allowed_user(42).unwrap(),
            "old invitation must not revoke a newer grant"
        );
        assert_eq!(store.identity(&token, 105).unwrap(), None);
        assert!(store.revoke_user(42).unwrap());
        assert!(!store.allowed_user(42).unwrap());
    }

    #[test]
    fn configured_owner_skips_invite_consumption_and_keeps_telegram_role() {
        let (_dir, store) = setup();
        store.configure_owner(Some(42)).unwrap();
        let invite = store.create_invite("Friend", 500, 100).unwrap();
        let login = approve(&store, Some(&invite.token), 42);
        let LoginPoll::Approved { token, .. } =
            store.poll_telegram_login(&login.cookie, 103).unwrap()
        else {
            panic!("not approved");
        };
        assert_eq!(
            store.identity(&token, 103).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert!(store.read().unwrap().usable_invite(&invite.id, 103));
        assert!(store.revoke_user(42).is_err());
        store.configure_owner(None).unwrap();
        assert!(!store.allowed_user(42).unwrap());
        assert_eq!(store.identity(&token, 104).unwrap(), None);
        store.configure_owner(Some(42)).unwrap();
        assert_eq!(store.identity(&token, 104).unwrap(), None);
        assert!(store.configure_owner(Some(0)).is_err());
        assert!(store.configure_owner(Some(u64::MAX)).is_err());
    }

    #[test]
    fn invite_inputs_and_storage_are_bounded() {
        let (_dir, store) = setup();
        for label in ["".to_string(), " ".to_string(), "x".repeat(129)] {
            assert!(store.create_invite(&label, 500, 100).is_err());
        }
        for expiry in [100, 99, 100 + MAX_INVITE_SECONDS + 1] {
            assert!(store.create_invite("Friend", expiry, 100).is_err());
        }
        assert!(!store.allowed_user(0).unwrap());
        assert!(!store.allowed_user(u64::MAX).unwrap());
        let mut journal = store.read().unwrap();
        for id in 0..MAX_INVITES {
            journal.invites.insert(
                format!("{id:064x}"),
                StoredInvite {
                    label: "Friend".into(),
                    expires: 500,
                    user_id: None,
                    revoked: false,
                },
            );
        }
        store.save(&journal).unwrap();
        assert!(store.create_invite("One too many", 500, 100).is_err());
        journal
            .telegram_bots
            .insert("x".repeat(1024 * 1024), "TestBot".into());
        assert!(store.save(&journal).is_err());
        assert_eq!(store.read().unwrap().invites.len(), MAX_INVITES);
    }
}
