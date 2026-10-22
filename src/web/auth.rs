use super::{
    Settings,
    pages::{checked_dir, data_root, open_regular, reject_symlinks},
};
use anyhow::ensure;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;
#[path = "auth_invites.rs"]
mod invites;
pub(crate) use invites::InvalidInvite;
#[path = "auth_telegram.rs"]
mod telegram;
pub(crate) use telegram::{LoginPoll, browser_login_cookie, browser_login_token};

const COOKIE: &str = "kodrik_web_session";
const SESSION_SECONDS: i64 = 7 * 86400;
const SESSIONS_PER_IDENTITY: usize = 4;
const USER_SESSIONS: usize = 64;
const MAX_SESSIONS: usize = USER_SESSIONS + SESSIONS_PER_IDENTITY;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum Identity {
    Owner,
    Telegram(u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    identity: Identity,
    expires: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredSession {
    // Existing password cookies retain their original expiry and owner identity.
    LegacyOwner(i64),
    Bound(Grant),
}

impl StoredSession {
    fn grant(&self) -> Grant {
        match self {
            Self::LegacyOwner(expires) => Grant {
                identity: Identity::Owner,
                expires: *expires,
            },
            Self::Bound(grant) => Grant {
                identity: grant.identity.clone(),
                expires: grant.expires,
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct AuthStore {
    root: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    password_hash: Option<String>,
    generation: Option<Uuid>,
    sessions: BTreeMap<String, StoredSession>,
    #[serde(default, rename = "codes", skip_serializing)]
    _obsolete_codes: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    telegram_bots: BTreeMap<String, String>,
    #[serde(default)]
    telegram_requests: BTreeMap<String, telegram::LoginRequest>,
    #[serde(default)]
    owner_id: Option<u64>,
    #[serde(default)]
    invites: BTreeMap<String, invites::StoredInvite>,
    #[serde(default)]
    members: BTreeMap<u64, String>,
}

impl Journal {
    fn create_session(&mut self, identity: Identity, now: i64) -> anyhow::Result<Option<String>> {
        if !self.allowed_identity(&identity) {
            return Ok(None);
        }
        let expires = now
            .checked_add(SESSION_SECONDS)
            .ok_or_else(|| anyhow::anyhow!("Invalid session time"))?;
        self.sessions
            .retain(|_, session| session.grant().expires > now);
        let mut own: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(key, session)| {
                let grant = session.grant();
                (grant.identity == identity).then_some((grant.expires, key.clone()))
            })
            .collect();
        own.sort();
        let pool_full = self.sessions.len() >= MAX_SESSIONS
            || (identity != Identity::Owner
                && self
                    .sessions
                    .values()
                    .filter(|session| session.grant().identity != Identity::Owner)
                    .count()
                    >= USER_SESSIONS);
        if pool_full && own.is_empty() {
            return Ok(None);
        }
        let remove = own
            .len()
            .saturating_sub(SESSIONS_PER_IDENTITY - 1)
            .max(usize::from(pool_full));
        for (_, key) in own.into_iter().take(remove) {
            self.sessions.remove(&key);
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        self.sessions.insert(
            token_hash(&token),
            StoredSession::Bound(Grant { identity, expires }),
        );
        Ok(Some(token))
    }
}

fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn valid_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl AuthStore {
    pub(crate) fn new(data: &Path) -> Self {
        Self {
            root: data_root(data).join("web"),
        }
    }

    fn lock(&self) -> anyhow::Result<File> {
        checked_dir(&self.root)?;
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        let path = self.root.join("auth.lock");
        reject_symlinks(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(file.metadata()?.is_file(), "Invalid auth lock");
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.lock()?;
        Ok(file)
    }

    fn read(&self) -> anyhow::Result<Journal> {
        let file = match open_regular(&self.root.join("auth.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Journal::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "Auth journal exceeds limit");
        let journal: Journal = serde_json::from_slice(&bytes)?;
        ensure!(
            journal.password_hash.is_some() == journal.generation.is_some()
                && journal.valid_membership()
                && journal.sessions.len() <= MAX_SESSIONS
                && journal.telegram_bots.len() <= 64
                && journal.telegram_requests.len() <= 64
                && journal.telegram_requests.keys().all(|key| valid_token(key))
                && journal
                    .telegram_requests
                    .values()
                    .all(telegram::LoginRequest::valid)
                && journal.sessions.keys().all(|key| valid_token(key)),
            "Invalid auth journal"
        );
        Ok(journal)
    }

    fn save(&self, journal: &Journal) -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        let bytes = serde_json::to_vec(journal)?;
        ensure!(bytes.len() <= 1024 * 1024, "Auth journal exceeds limit");
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("auth.json"))?;
        Ok(())
    }

    pub(crate) fn set_password(&self, password: &str) -> anyhow::Result<()> {
        ensure!(
            !password.is_empty() && password.len() <= 1024,
            "Password must contain 1–1024 bytes"
        );
        let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)?
            .to_string();
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        journal.password_hash = Some(hash);
        journal.generation = Some(Uuid::new_v4());
        journal.sessions.clear();
        journal.telegram_requests.clear();
        self.save(&journal)
    }

    pub(crate) fn login(&self, password: &str, now: i64) -> anyhow::Result<Option<String>> {
        if password.is_empty() || password.len() > 1024 {
            return Ok(None);
        }
        let snapshot = {
            let _lock = self.lock()?;
            self.read()?
        };
        let Some(hash) = snapshot.password_hash else {
            return Ok(None);
        };
        let parsed = PasswordHash::new(&hash)?;
        match Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => {}
            Err(argon2::password_hash::Error::Password) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        if journal.generation != snapshot.generation {
            return Ok(None);
        }
        let token = journal.create_session(Identity::Owner, now)?;
        self.save(&journal)?;
        Ok(token)
    }

    pub(crate) fn authenticated(&self, token: &str, now: i64) -> anyhow::Result<bool> {
        Ok(self.identity(token, now)? == Some(Identity::Owner))
    }

    pub(crate) fn identity(&self, token: &str, now: i64) -> anyhow::Result<Option<Identity>> {
        if !valid_token(token) {
            return Ok(None);
        }
        let journal = self.read()?;
        Ok(journal
            .sessions
            .get(&token_hash(token))
            .map(StoredSession::grant)
            .filter(|grant| grant.expires > now && journal.allowed_identity(&grant.identity))
            .map(|grant| grant.identity))
    }

    #[cfg(test)]
    pub(crate) fn test_session(
        &self,
        identity: Identity,
        now: i64,
    ) -> anyhow::Result<Option<String>> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let token = journal.create_session(identity, now)?;
        self.save(&journal)?;
        Ok(token)
    }

    pub(crate) fn logout(&self, token: &str) -> anyhow::Result<()> {
        if !valid_token(token) {
            return Ok(());
        }
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        journal.sessions.remove(&token_hash(token));
        self.save(&journal)
    }
}

pub(crate) fn session_cookie(settings: &Settings, token: &str, remove: bool) -> String {
    cookie::Cookie::build((COOKIE, token))
        .path(settings.prefix())
        .http_only(true)
        .secure(settings.secure())
        .same_site(cookie::SameSite::Lax)
        .max_age(cookie::time::Duration::seconds(if remove {
            0
        } else {
            SESSION_SECONDS
        }))
        .build()
        .to_string()
}

pub(crate) fn request_token(headers: &HeaderMap) -> Option<String> {
    let mut token = None;
    for header in headers.get_all(header::COOKIE) {
        for cookie in cookie::Cookie::split_parse(header.to_str().ok()?) {
            let cookie = cookie.ok()?;
            if cookie.name() == COOKIE {
                if token.is_some() || !valid_token(cookie.value()) {
                    return None;
                }
                token = Some(cookie.value().to_string());
            }
        }
    }
    token
}

pub(crate) fn safe_return_path(settings: &Settings, path: &str) -> String {
    let route = path.split('?').next().unwrap_or("");
    if !path.starts_with(settings.prefix())
        || path.contains(['\\', '%', '#'])
        || path.chars().any(|c| c.is_control() || c.is_whitespace())
        || route.contains("//")
        || route.split('/').any(|part| part == "." || part == "..")
        || route.starts_with(&format!("{}auth/", settings.prefix()))
    {
        settings.prefix().to_string()
    } else {
        path.to_string()
    }
}

pub(crate) fn origin_allowed(settings: &Settings, origin: Option<&str>) -> bool {
    origin.is_some_and(|value| value == settings.origin())
}

const LOGIN_CLIENTS: usize = 1024;

pub(crate) struct LoginLimiter {
    attempts: BTreeMap<IpAddr, VecDeque<Instant>>,
}
impl LoginLimiter {
    pub(crate) fn new() -> Self {
        Self {
            attempts: BTreeMap::new(),
        }
    }
    pub(crate) fn allow(&mut self, client: IpAddr, now: Instant) -> bool {
        self.attempts.retain(|_, attempts| {
            while attempts
                .front()
                .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
            {
                attempts.pop_front();
            }
            !attempts.is_empty()
        });
        // Never evict an active client: rotating addresses must not reset its budget.
        if self.attempts.len() >= LOGIN_CLIENTS && !self.attempts.contains_key(&client) {
            return false;
        }
        let attempts = self.attempts.entry(client).or_default();
        if attempts.len() >= 5 {
            return false;
        }
        attempts.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebConfig;

    #[test]
    fn legacy_owner_sessions_cannot_overflow_the_journal_on_user_login() {
        let mut journal = Journal::default();
        for id in 0..64 {
            journal
                .sessions
                .insert(format!("{id:064x}"), StoredSession::LegacyOwner(1000));
        }
        for id in 1..=5 {
            journal.members.insert(id, "test".into());
        }
        for id in 1..5 {
            assert!(
                journal
                    .create_session(Identity::Telegram(id), 100)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            journal
                .create_session(Identity::Telegram(5), 100)
                .unwrap()
                .is_none()
        );
        assert_eq!(journal.sessions.len(), MAX_SESSIONS);
        assert!(
            journal
                .create_session(Identity::Owner, 100)
                .unwrap()
                .is_some()
        );
        assert_eq!(journal.sessions.len(), 8);
    }

    #[test]
    fn session_reservations_preserve_other_identities_and_rotate_only_own_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        for user in 1..=65 {
            store.grant_test_user(user).unwrap();
        }
        store.set_password("owner-password").unwrap();
        let mut users = Vec::new();
        for id in 1..=64 {
            users.push(
                store
                    .test_session(Identity::Telegram(id), 100)
                    .unwrap()
                    .unwrap(),
            );
        }
        let owner = store.login("owner-password", 101).unwrap().unwrap();
        for now in 102..106 {
            assert!(store.test_session(Identity::Owner, now).unwrap().is_some());
        }
        assert_eq!(store.identity(&owner, 106).unwrap(), None);
        assert_eq!(store.read().unwrap().sessions.len(), MAX_SESSIONS);
        assert!(
            store
                .test_session(Identity::Telegram(1), 106)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.identity(&users[0], 107).unwrap(), None);
        for (id, token) in users.iter().enumerate().skip(1) {
            assert_eq!(
                store.identity(token, 107).unwrap(),
                Some(Identity::Telegram(id as u64 + 1))
            );
        }
        assert_eq!(store.read().unwrap().sessions.len(), MAX_SESSIONS);
        assert!(
            store
                .test_session(Identity::Telegram(65), 107)
                .unwrap()
                .is_none()
        );
        store.logout(&users[1]).unwrap();
        assert!(
            store
                .test_session(Identity::Telegram(65), 107)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn login_limiter_bounds_clients_without_resetting_active_budgets() {
        let mut limiter = LoginLimiter::new();
        let now = Instant::now();
        let first = IpAddr::from([192, 0, 2, 1]);
        for _ in 0..5 {
            assert!(limiter.allow(first, now));
        }
        for id in 1..LOGIN_CLIENTS {
            assert!(limiter.allow(std::net::Ipv6Addr::from(id as u128).into(), now));
        }
        assert!(!limiter.allow(first, now));
        assert!(!limiter.allow(std::net::Ipv6Addr::from(9999u128).into(), now));
        assert_eq!(limiter.attempts.len(), LOGIN_CLIENTS);
        assert!(limiter.allow(
            std::net::Ipv6Addr::from(9999u128).into(),
            now + Duration::from_secs(60)
        ));
        assert_eq!(limiter.attempts.len(), 1);
    }

    #[test]
    fn repeated_user_logins_do_not_block_owner_or_other_users() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(7).unwrap();
        store.grant_test_user(8).unwrap();
        for now in 100..164 {
            assert!(
                store
                    .test_session(Identity::Telegram(7), now)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(store.test_session(Identity::Owner, 164).unwrap().is_some());
        assert!(
            store
                .test_session(Identity::Telegram(8), 164)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.read().unwrap().sessions.len(), 6);
    }

    #[test]
    fn sessions_persist_bind_identity_expire_and_logout() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(42).unwrap();
        store.grant_test_user(43).unwrap();
        let token = store
            .test_session(Identity::Telegram(42), 101)
            .unwrap()
            .unwrap();
        let other = store
            .test_session(Identity::Telegram(43), 102)
            .unwrap()
            .unwrap();
        assert_ne!(token, other);
        assert!(valid_token(&token));
        let reopened = AuthStore::new(dir.path());
        assert_eq!(
            reopened.identity(&token, 102).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert!(!store.authenticated(&token, 102).unwrap());
        assert_eq!(
            store.identity(&other, 103).unwrap(),
            Some(Identity::Telegram(43))
        );
        assert_eq!(store.identity(&token, 101 + SESSION_SECONDS).unwrap(), None);
        let journal = fs::read_to_string(dir.path().join("web/auth.json")).unwrap();
        assert!(!journal.contains(&token));
        store.logout(&token).unwrap();
        assert_eq!(reopened.identity(&token, 102).unwrap(), None);
        assert_eq!(store.identity("invalid", 102).unwrap(), None);
    }

    #[test]
    fn password_reset_revokes_all_sessions_and_keeps_owner_page_access() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.grant_test_user(42).unwrap();
        let token = store.test_session(Identity::Owner, 701).unwrap().unwrap();
        assert!(store.authenticated(&token, 702).unwrap());
        assert_eq!(store.identity(&token, 702).unwrap(), Some(Identity::Owner));
        let user_token = store
            .test_session(Identity::Telegram(42), 702)
            .unwrap()
            .unwrap();
        store.set_password("password").unwrap();
        assert_eq!(store.identity(&token, 703).unwrap(), None);
        assert_eq!(store.identity(&user_token, 703).unwrap(), None);
        let password_token = store.login("password", 703).unwrap().unwrap();
        assert_eq!(
            store.identity(&password_token, 704).unwrap(),
            Some(Identity::Owner)
        );
    }

    #[test]
    fn legacy_password_sessions_remain_owner_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.set_password("password").unwrap();
        let token = "a".repeat(64);
        let path = dir.path().join("web/auth.json");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        journal["sessions"] = serde_json::json!({token_hash(&token): 1000});
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert_eq!(store.identity(&token, 100).unwrap(), Some(Identity::Owner));
        assert!(store.authenticated(&token, 100).unwrap());
    }

    #[test]
    fn session_counts_are_bounded_and_expired_sessions_free_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        for user in 1..=65 {
            store.grant_test_user(user).unwrap();
        }
        for id in 1..=64 {
            assert!(
                store
                    .test_session(Identity::Telegram(id), 701)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(store.test_session(Identity::Owner, 701).unwrap().is_some());
        assert_eq!(store.read().unwrap().sessions.len(), 65);
        assert_eq!(
            store.test_session(Identity::Telegram(65), 701).unwrap(),
            None
        );
        assert!(
            store
                .test_session(Identity::Telegram(65), 701 + SESSION_SECONDS)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.read().unwrap().sessions.len(), 1);
    }

    #[test]
    fn obsolete_login_codes_are_discarded_without_losing_existing_auth_data() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        store.set_password("owner-password").unwrap();
        store.configure_owner(Some(7)).unwrap();
        store.grant_test_user(42).unwrap();
        let owner = store.test_session(Identity::Owner, 100).unwrap().unwrap();
        let user = store
            .test_session(Identity::Telegram(42), 100)
            .unwrap()
            .unwrap();
        store.create_invite("Next member", 500, 100).unwrap();
        store.register_telegram_bot("main", "TestBot").unwrap();
        let pending = store
            .begin_telegram_login(None, "/chat/", 100)
            .unwrap()
            .unwrap();
        let path = dir.path().join("web/auth.json");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        journal["codes"] =
            serde_json::json!({"a".repeat(64): {"identity":{"Telegram":42},"expires":1000}});
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let reopened = AuthStore::new(dir.path());
        assert_eq!(
            reopened.identity(&owner, 101).unwrap(),
            Some(Identity::Owner)
        );
        assert_eq!(
            reopened.identity(&user, 101).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert!(reopened.allowed_user(7).unwrap());
        assert!(matches!(
            reopened.poll_telegram_login(&pending.cookie, 101).unwrap(),
            LoginPoll::Pending
        ));
        reopened.logout(&"f".repeat(64)).unwrap();
        journal.as_object_mut().unwrap().remove("codes");
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, journal);
        assert!(reopened.login("owner-password", 102).unwrap().is_some());
        let mut invalid = saved;
        invalid["unexpected"] = serde_json::json!(true);
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(reopened.identity(&owner, 103).is_err());
    }

    #[test]
    fn password_sessions_persist_expire_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path());
        assert_eq!(store.login("wrong", 100).unwrap(), None);
        store.set_password("a-test-password").unwrap();
        assert_eq!(store.login("wrong", 100).unwrap(), None);
        let token = store.login("a-test-password", 100).unwrap().unwrap();
        let reopened = AuthStore::new(dir.path());
        assert!(reopened.authenticated(&token, 101).unwrap());
        assert!(!reopened.authenticated(&token, 100 + 7 * 86400).unwrap());
        let journal = std::fs::read_to_string(dir.path().join("web/auth.json")).unwrap();
        assert!(!journal.contains(&token));
        assert!(!journal.contains("a-test-password"));
        store.set_password("another-test-password").unwrap();
        assert!(!reopened.authenticated(&token, 101).unwrap());
        let token = store.login("another-test-password", 101).unwrap().unwrap();
        store.logout(&token).unwrap();
        assert!(!reopened.authenticated(&token, 102).unwrap());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("web/auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(store.set_password("").is_err());
        std::fs::write(dir.path().join("web/auth.json"), "broken").unwrap();
        assert!(store.authenticated(&token, 102).is_err());
    }

    #[test]
    fn cookies_redirects_origins_and_rate_limit_protect_login() {
        let settings = Settings::from_config(&WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: "0.0.0.0:8080".into(),
            base_url: Some("https://pages.example.com/kodrik/".into()),
        })
        .unwrap()
        .unwrap();
        let cookie = session_cookie(&settings, "test", false);
        for flag in [
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Path=/kodrik/",
            "Max-Age=604800",
        ] {
            assert!(cookie.contains(flag), "{cookie}");
        }
        assert!(!cookie.contains("Domain="));
        assert!(session_cookie(&settings, "", true).contains("Max-Age=0"));
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/other/",
            "/kodrik/../other",
            "/kodrik/%2f%2fevil",
            "/kodrik/auth/login",
            "/kodrik/x\\y",
        ] {
            assert_eq!(safe_return_path(&settings, bad), "/kodrik/", "{bad}");
        }
        assert_eq!(
            safe_return_path(&settings, "/kodrik/pages/test/"),
            "/kodrik/pages/test/"
        );
        assert!(origin_allowed(&settings, Some("https://pages.example.com")));
        assert!(!origin_allowed(&settings, None));
        assert!(!origin_allowed(&settings, Some("https://evil.example")));
        let mut limiter = LoginLimiter::new();
        let now = std::time::Instant::now();
        for _ in 0..5 {
            assert!(limiter.allow("127.0.0.1".parse().unwrap(), now));
        }
        assert!(!limiter.allow("127.0.0.1".parse().unwrap(), now));
        assert!(limiter.allow(
            "127.0.0.1".parse().unwrap(),
            now + std::time::Duration::from_secs(60)
        ));
    }
}
