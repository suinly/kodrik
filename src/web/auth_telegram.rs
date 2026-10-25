use super::*;

const LOGIN_SECONDS: i64 = 300;
const BROWSER_COOKIE: &str = "kodrik_telegram_login";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LoginRequest {
    browser_hash: String,
    interface: String,
    return_to: String,
    expires: i64,
    pub(super) user_id: Option<u64>,
    #[serde(default)]
    invite_hash: Option<String>,
    decision: Option<bool>,
}

impl LoginRequest {
    pub(super) fn valid(&self) -> bool {
        valid_token(&self.browser_hash)
            && self.expires > 0
            && !self.interface.is_empty()
            && self.interface.len() <= 256
            && self.return_to.len() <= 2048
            && self.user_id.is_none_or(invites::valid_user)
            && self
                .invite_hash
                .as_ref()
                .is_none_or(|hash| valid_token(hash))
            && (self.decision.is_none() || self.user_id.is_some())
    }
}

pub(crate) struct BrowserLogin {
    pub id: String,
    pub cookie: String,
    pub username: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoginPoll {
    Pending,
    Denied,
    Expired,
    Approved { token: String, return_to: String },
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn browser_parts(cookie: &str) -> Option<(&str, &str)> {
    let (id, secret) = cookie.split_once('.')?;
    (valid_id(id) && valid_token(secret)).then_some((id, secret))
}

impl AuthStore {
    pub(crate) fn clear_telegram_bots(&self) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        journal.telegram_bots.clear();
        self.save(&journal)
    }

    pub(crate) fn register_telegram_bot(
        &self,
        interface: &str,
        username: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            !interface.is_empty()
                && interface.len() <= 256
                && (5..=32).contains(&username.len())
                && username
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "Invalid Telegram bot identity"
        );
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        ensure!(
            journal.telegram_bots.contains_key(interface) || journal.telegram_bots.len() < 64,
            "Too many Telegram bots"
        );
        journal
            .telegram_bots
            .insert(interface.into(), username.into());
        self.save(&journal)
    }

    #[cfg(test)]
    pub(crate) fn begin_telegram_login(
        &self,
        previous: Option<&str>,
        return_to: &str,
        now: i64,
    ) -> anyhow::Result<Option<BrowserLogin>> {
        self.begin_telegram_login_with_invite(previous, return_to, None, now)
    }

    pub(crate) fn begin_telegram_login_with_invite(
        &self,
        previous: Option<&str>,
        return_to: &str,
        invite: Option<&str>,
        now: i64,
    ) -> anyhow::Result<Option<BrowserLogin>> {
        ensure!(return_to.len() <= 2048, "Login destination too long");
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let invite_hash = invite.map(token_hash);
        if let Some(token) = invite {
            if !invites::valid_invite_token(token)
                || !journal.usable_invite(invite_hash.as_ref().unwrap(), now)
            {
                return Err(InvalidInvite.into());
            }
        }
        let Some((interface, username)) = journal
            .telegram_bots
            .first_key_value()
            .map(|(a, b)| (a.clone(), b.clone()))
        else {
            return Ok(None);
        };
        journal
            .telegram_requests
            .retain(|_, request| request.expires > now);
        if let Some((id, secret)) = previous.and_then(browser_parts) {
            let key = token_hash(id);
            if journal
                .telegram_requests
                .get(&key)
                .is_some_and(|request| request.browser_hash == token_hash(secret))
            {
                journal.telegram_requests.remove(&key);
            }
        }
        ensure!(
            journal.telegram_requests.len() < 64,
            "Too many pending Telegram logins"
        );
        let id = Uuid::new_v4().simple().to_string();
        let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        journal.telegram_requests.insert(
            token_hash(&id),
            LoginRequest {
                browser_hash: token_hash(&secret),
                interface,
                return_to: return_to.into(),
                expires: now
                    .checked_add(LOGIN_SECONDS)
                    .ok_or_else(|| anyhow::anyhow!("Invalid login time"))?,
                user_id: None,
                invite_hash,
                decision: None,
            },
        );
        self.save(&journal)?;
        Ok(Some(BrowserLogin {
            cookie: format!("{id}.{secret}"),
            id,
            username,
        }))
    }

    pub(crate) fn present_telegram_login(
        &self,
        id: &str,
        interface: &str,
        user_id: u64,
        now: i64,
    ) -> anyhow::Result<Option<String>> {
        if !valid_id(id) || !invites::valid_user(user_id) {
            return Ok(None);
        }
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let allowed = journal.allowed_user(user_id);
        let Some(request) = journal.telegram_requests.get(&token_hash(id)) else {
            return Ok(None);
        };
        let eligible = allowed
            || request
                .invite_hash
                .as_ref()
                .is_some_and(|hash| journal.usable_invite(hash, now));
        if request.expires <= now
            || request.interface != interface
            || request.decision.is_some()
            || request.user_id.is_some_and(|user| user != user_id)
        {
            return Ok(None);
        }
        let request = journal.telegram_requests.get_mut(&token_hash(id)).unwrap();
        request.user_id = Some(user_id);
        if !eligible {
            request.decision = Some(false);
        }
        self.save(&journal)?;
        Ok(eligible.then(|| id[..6].to_ascii_uppercase()))
    }

    pub(crate) fn decide_telegram_login(
        &self,
        id: &str,
        interface: &str,
        user_id: u64,
        approve: bool,
        now: i64,
    ) -> anyhow::Result<bool> {
        if !valid_id(id) || !invites::valid_user(user_id) {
            return Ok(false);
        }
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let allowed = journal.allowed_user(user_id);
        let Some(request) = journal.telegram_requests.get(&token_hash(id)) else {
            return Ok(false);
        };
        if approve
            && !allowed
            && !request
                .invite_hash
                .as_ref()
                .is_some_and(|hash| journal.usable_invite(hash, now))
        {
            return Ok(false);
        }
        let Some(request) = journal.telegram_requests.get_mut(&token_hash(id)) else {
            return Ok(false);
        };
        if request.expires <= now
            || request.interface != interface
            || request.user_id != Some(user_id)
            || request.decision.is_some_and(|decision| decision != approve)
        {
            return Ok(false);
        }
        request.decision = Some(approve);
        self.save(&journal)?;
        Ok(true)
    }

    pub(crate) fn cancel_telegram_login(&self, cookie: &str) -> anyhow::Result<()> {
        let Some((id, secret)) = browser_parts(cookie) else {
            return Ok(());
        };
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let key = token_hash(id);
        if journal
            .telegram_requests
            .get(&key)
            .is_some_and(|request| request.browser_hash == token_hash(secret))
        {
            journal.telegram_requests.remove(&key);
            self.save(&journal)?;
        }
        Ok(())
    }

    pub(crate) fn poll_telegram_login(&self, cookie: &str, now: i64) -> anyhow::Result<LoginPoll> {
        let Some((id, secret)) = browser_parts(cookie) else {
            return Ok(LoginPoll::Expired);
        };
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let key = token_hash(id);
        let Some(request) = journal.telegram_requests.get(&key) else {
            return Ok(LoginPoll::Expired);
        };
        if request.browser_hash != token_hash(secret) {
            return Ok(LoginPoll::Expired);
        }
        if request.expires <= now {
            journal.telegram_requests.remove(&key);
            self.save(&journal)?;
            return Ok(LoginPoll::Expired);
        }
        if request
            .invite_hash
            .as_ref()
            .is_some_and(|hash| !journal.usable_invite(hash, now))
            && !request
                .user_id
                .is_some_and(|user| journal.allowed_user(user))
        {
            journal.telegram_requests.remove(&key);
            self.save(&journal)?;
            return Ok(LoginPoll::Denied);
        }
        let Some(decision) = request.decision else {
            return Ok(LoginPoll::Pending);
        };
        if !decision {
            journal.telegram_requests.remove(&key);
            self.save(&journal)?;
            return Ok(LoginPoll::Denied);
        }
        let user = request.user_id.unwrap();
        let invite = request
            .invite_hash
            .clone()
            .filter(|hash| journal.usable_invite(hash, now));
        if !journal.allowed_user(user) && invite.is_none() {
            journal.telegram_requests.remove(&key);
            self.save(&journal)?;
            return Ok(LoginPoll::Denied);
        }
        if journal.owner_id != Some(user)
            && let Some(hash) = invite
        {
            ensure!(
                journal.members.contains_key(&user) || journal.members.len() < 1024,
                "Too many members"
            );
            journal.invites.get_mut(&hash).unwrap().user_id = Some(user);
            journal.members.insert(user, hash);
        }
        let identity = Identity::Telegram(user);
        let token = journal
            .create_session(identity, now)?
            .ok_or_else(|| anyhow::anyhow!("Too many browser sessions"))?;
        let request = journal.telegram_requests.remove(&key).unwrap();
        self.save(&journal)?;
        Ok(LoginPoll::Approved {
            token,
            return_to: request.return_to,
        })
    }
}

pub(crate) fn browser_login_cookie(settings: &Settings, value: &str, remove: bool) -> String {
    cookie::Cookie::build((BROWSER_COOKIE, value))
        .path(settings.prefix())
        .http_only(true)
        .secure(settings.secure())
        .same_site(cookie::SameSite::Strict)
        .max_age(cookie::time::Duration::seconds(if remove {
            0
        } else {
            LOGIN_SECONDS
        }))
        .build()
        .to_string()
}

pub(crate) fn browser_login_token(headers: &HeaderMap) -> Option<String> {
    let mut value = None;
    for header in headers.get_all(header::COOKIE) {
        for cookie in cookie::Cookie::split_parse(header.to_str().ok()?) {
            let cookie = cookie.ok()?;
            if cookie.name() == BROWSER_COOKIE {
                if value.is_some() || browser_parts(cookie.value()).is_none() {
                    return None;
                }
                value = Some(cookie.value().to_owned());
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_browser_logins_rotate_only_that_users_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(7).unwrap();
        store.grant_test_user(42).unwrap();
        store.register_telegram_bot("main", "TestBot").unwrap();
        let owner = store.test_session(Identity::Owner, 100).unwrap().unwrap();
        let mut tokens = Vec::new();
        for now in 101..107 {
            let login = store
                .begin_telegram_login(None, "/chat/", now)
                .unwrap()
                .unwrap();
            store
                .present_telegram_login(&login.id, "main", 7, now)
                .unwrap();
            assert!(
                store
                    .decide_telegram_login(&login.id, "main", 7, true, now)
                    .unwrap()
            );
            let LoginPoll::Approved { token, .. } =
                store.poll_telegram_login(&login.cookie, now).unwrap()
            else {
                panic!("login not approved")
            };
            tokens.push(token);
        }
        assert_eq!(store.read().unwrap().sessions.len(), 5);
        assert_eq!(store.identity(&owner, 107).unwrap(), Some(Identity::Owner));
        assert_eq!(store.identity(&tokens[0], 107).unwrap(), None);
        assert_eq!(
            store.identity(&tokens[5], 107).unwrap(),
            Some(Identity::Telegram(7))
        );
    }

    #[test]
    fn telegram_login_denial_expiry_cancellation_and_password_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(7).unwrap();
        store.grant_test_user(42).unwrap();
        assert!(
            store
                .begin_telegram_login(None, "/", 100)
                .unwrap()
                .is_none()
        );
        store.register_telegram_bot("main", "TestBot").unwrap();
        let denied = store.begin_telegram_login(None, "/", 100).unwrap().unwrap();
        store
            .present_telegram_login(&denied.id, "main", 7, 101)
            .unwrap();
        assert!(
            store
                .decide_telegram_login(&denied.id, "main", 7, false, 101)
                .unwrap()
        );
        assert!(
            !store
                .decide_telegram_login(&denied.id, "main", 7, true, 102)
                .unwrap()
        );
        assert_eq!(
            store.poll_telegram_login(&denied.cookie, 102).unwrap(),
            LoginPoll::Denied
        );
        let old = store.begin_telegram_login(None, "/", 100).unwrap().unwrap();
        let new = store
            .begin_telegram_login(Some(&old.cookie), "/", 101)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.poll_telegram_login(&old.cookie, 102).unwrap(),
            LoginPoll::Expired
        );
        store.cancel_telegram_login(&new.cookie).unwrap();
        assert_eq!(
            store.poll_telegram_login(&new.cookie, 102).unwrap(),
            LoginPoll::Expired
        );
        let expired = store.begin_telegram_login(None, "/", 100).unwrap().unwrap();
        assert!(
            store
                .present_telegram_login(&expired.id, "main", 7, 400)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.poll_telegram_login(&expired.cookie, 400).unwrap(),
            LoginPoll::Expired
        );
        let revoked = store.begin_telegram_login(None, "/", 400).unwrap().unwrap();
        store.set_password("new secure passphrase").unwrap();
        assert_eq!(
            store.poll_telegram_login(&revoked.cookie, 401).unwrap(),
            LoginPoll::Expired
        );
        assert!(
            store
                .begin_telegram_login(None, "/", 401)
                .unwrap()
                .is_some(),
            "password changes preserve bot discovery"
        );
    }

    #[test]
    fn telegram_login_has_bounded_pending_requests_and_one_concurrent_redemption() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(7).unwrap();
        store.grant_test_user(42).unwrap();
        store.register_telegram_bot("main", "TestBot").unwrap();
        let login = store.begin_telegram_login(None, "/", 100).unwrap().unwrap();
        store
            .present_telegram_login(&login.id, "main", 7, 101)
            .unwrap();
        store
            .decide_telegram_login(&login.id, "main", 7, true, 101)
            .unwrap();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| store.poll_telegram_login(&login.cookie, 102).unwrap());
            let second = scope.spawn(|| store.poll_telegram_login(&login.cookie, 102).unwrap());
            let results = [first.join().unwrap(), second.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, LoginPoll::Approved { .. }))
                    .count(),
                1
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, LoginPoll::Expired))
                    .count(),
                1
            );
        });
        for _ in 0..64 {
            store.begin_telegram_login(None, "/", 103).unwrap().unwrap();
        }
        assert!(store.begin_telegram_login(None, "/", 103).is_err());
        assert!(
            store
                .begin_telegram_login(None, "/", 403)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn telegram_login_requires_browser_secret_private_identity_and_explicit_approval() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(7).unwrap();
        store.grant_test_user(42).unwrap();
        store.register_telegram_bot("main", "TestBot").unwrap();
        let login = store
            .begin_telegram_login(None, "/chat/", 100)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.poll_telegram_login(&login.cookie, 101).unwrap(),
            LoginPoll::Pending
        );
        assert!(
            !store
                .decide_telegram_login(&login.id, "main", 42, true, 101)
                .unwrap()
        );
        assert!(
            store
                .present_telegram_login(&login.id, "wrong", 42, 101)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .present_telegram_login(&login.id, "main", 0, 101)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .present_telegram_login(&login.id, "main", 42, 101)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .present_telegram_login(&login.id, "main", 7, 101)
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .decide_telegram_login(&login.id, "main", 7, true, 101)
                .unwrap()
        );
        assert!(
            !store
                .decide_telegram_login(&login.id, "wrong", 42, true, 101)
                .unwrap()
        );
        assert_eq!(
            store
                .poll_telegram_login(&format!("{}.{}", login.id, "b".repeat(64)), 101)
                .unwrap(),
            LoginPoll::Expired
        );
        assert!(
            store
                .decide_telegram_login(&login.id, "main", 42, true, 102)
                .unwrap()
        );
        let reopened = AuthStore::new(dir.path());
        let LoginPoll::Approved { token, return_to } =
            reopened.poll_telegram_login(&login.cookie, 103).unwrap()
        else {
            panic!("approval did not produce a session")
        };
        assert_eq!(return_to, "/chat/");
        assert_eq!(
            store.identity(&token, 103).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert!(!store.authenticated(&token, 103).unwrap());
        assert_eq!(
            store.poll_telegram_login(&login.cookie, 103).unwrap(),
            LoginPoll::Expired
        );
        let journal = fs::read_to_string(dir.path().join("web/auth.json")).unwrap();
        for secret in [&login.id, &login.cookie, &token] {
            assert!(!journal.contains(secret));
        }
    }
}
