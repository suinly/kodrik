use super::*;

const PAIR_SECONDS: i64 = 300;
const DEVICE_SECONDS: i64 = 30 * 86400;
const DEVICES_PER_ACCOUNT: usize = 16;
const MAX_GUEST_DEVICES: usize = 256;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredDevice {
    label: String,
    token_hash: String,
    expires_at: i64,
    // Old journal entries belong to the gateway owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredPairing {
    label: String,
    code_hash: String,
    polling_hash: String,
    expires_at: i64,
    approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<u64>,
    last_poll: Option<i64>,
}

#[derive(Debug)]
pub(crate) enum DeviceError {
    Invalid,
    Unauthorized,
    Forbidden,
    RateLimited,
    Capacity,
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Device request denied")
    }
}
impl std::error::Error for DeviceError {}

#[derive(Serialize)]
pub(crate) struct DeviceView {
    pub id: Uuid,
    pub label: String,
    pub expires_at: i64,
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.trim() == label
        && label.chars().count() <= 128
        && !label.chars().any(char::is_control)
}

impl Journal {
    pub(super) fn valid_devices(&self) -> bool {
        self.devices.len() <= DEVICES_PER_ACCOUNT + MAX_GUEST_DEVICES
            && self.device_pairings.len() <= 32
            && self.device_approval_attempts.len() <= 5
            && self.guest_device_approval_attempts.len() <= self.members.len()
            && self
                .guest_device_approval_attempts
                .iter()
                .all(|(&user, attempts)| self.members.contains_key(&user) && attempts.len() <= 5)
            && self.devices.values().all(|device| {
                valid_label(&device.label)
                    && valid_token(&device.token_hash)
                    && device.expires_at > 0
                    && self.device_account_active(device.account)
            })
            && self.device_pairings.values().all(|request| {
                valid_label(&request.label)
                    && valid_token(&request.code_hash)
                    && valid_token(&request.polling_hash)
                    && request.expires_at > 0
                    && self.device_account_active(request.account)
                    && (request.approved || request.account.is_none())
            })
    }

    fn device_browser_identity(&self, token: &str, now: i64) -> anyhow::Result<Identity> {
        ensure!(valid_token(token), DeviceError::Unauthorized);
        self.identity(token, now)
            .ok_or_else(|| DeviceError::Unauthorized.into())
    }

    fn device_account(&self, identity: &Identity) -> anyhow::Result<Option<u64>> {
        match identity {
            Identity::Owner => Ok(None),
            Identity::Telegram(id) if self.owner_id == Some(*id) => Ok(None),
            Identity::Telegram(id) if self.allowed_user(*id) => Ok(Some(*id)),
            _ => Err(DeviceError::Forbidden.into()),
        }
    }

    fn device_account_active(&self, account: Option<u64>) -> bool {
        account.is_none_or(|id| self.owner_id != Some(id) && self.allowed_user(id))
    }

    pub(super) fn revoke_user_devices(&mut self, user: u64) {
        self.devices
            .retain(|_, device| device.account != Some(user));
        self.device_pairings
            .retain(|_, request| request.account != Some(user));
        self.guest_device_approval_attempts.remove(&user);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DeviceIdentity {
    pub device_id: Uuid,
    pub account: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct PairingStarted {
    pub request_id: Uuid,
    pub code: String,
    pub polling_secret: String,
    pub expires_at: i64,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum PairingPoll {
    Pending,
    Authorized { device_id: Uuid, token: String },
    Expired,
}

impl AuthStore {
    pub(crate) fn start_device_pairing(
        &self,
        label: &str,
        now: i64,
    ) -> anyhow::Result<PairingStarted> {
        ensure!(valid_label(label), DeviceError::Invalid);
        let expires_at = now
            .checked_add(PAIR_SECONDS)
            .filter(|_| now > 0)
            .ok_or(DeviceError::Invalid)?;
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        journal
            .device_pairings
            .retain(|_, request| request.expires_at > now);
        ensure!(journal.device_pairings.len() < 32, DeviceError::Capacity);
        let request_id = Uuid::new_v4();
        let polling_secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let random = Uuid::new_v4();
        // Skip UUID version/variant bytes; low five random bits select 32 symbols evenly.
        let code: String = [0, 1, 2, 3, 4, 5, 9, 10]
            .iter()
            .map(|index| ALPHABET[usize::from(random.as_bytes()[*index]) & 31] as char)
            .collect();
        journal.device_pairings.insert(
            request_id,
            StoredPairing {
                label: label.into(),
                code_hash: token_hash(&code),
                polling_hash: token_hash(&polling_secret),
                expires_at,
                approved: false,
                account: None,
                last_poll: None,
            },
        );
        self.save(&journal)?;
        Ok(PairingStarted {
            request_id,
            code,
            polling_secret,
            expires_at,
        })
    }
    #[cfg(test)]
    pub(crate) fn approve_device_pairing(
        &self,
        id: Uuid,
        code: &str,
        identity: &Identity,
        now: i64,
    ) -> anyhow::Result<()> {
        self.approve_device_pairing_as(id, code, now, |_| Ok(identity.clone()))
    }

    pub(crate) fn approve_device_pairing_from_session(
        &self,
        id: Uuid,
        code: &str,
        token: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.approve_device_pairing_as(id, code, now, |journal| {
            journal.device_browser_identity(token, now)
        })
    }

    fn approve_device_pairing_as(
        &self,
        id: Uuid,
        code: &str,
        now: i64,
        identity: impl FnOnce(&Journal) -> anyhow::Result<Identity>,
    ) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let account = journal.device_account(&identity(&journal)?)?;
        let attempts = match account {
            None => &mut journal.device_approval_attempts,
            Some(user) => journal
                .guest_device_approval_attempts
                .entry(user)
                .or_default(),
        };
        attempts.retain(|at| now.saturating_sub(*at) < 60);
        ensure!(attempts.len() < 5, DeviceError::RateLimited);
        attempts.push_back(now);
        // Failed guesses also consume the authenticated account's budget.
        self.save(&journal)?;
        let request = journal
            .device_pairings
            .get_mut(&id)
            .ok_or(DeviceError::Invalid)?;
        ensure!(
            request.expires_at > now
                && !request.approved
                && code.len() == 8
                && request.code_hash == token_hash(code),
            DeviceError::Invalid
        );
        request.account = account;
        request.approved = true;
        self.save(&journal)
    }
    pub(crate) fn poll_device_pairing(
        &self,
        id: Uuid,
        secret: &str,
        now: i64,
    ) -> anyhow::Result<PairingPoll> {
        ensure!(valid_token(secret), DeviceError::Invalid);
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let Some(request) = journal.device_pairings.get_mut(&id) else {
            return Ok(PairingPoll::Expired);
        };
        ensure!(
            request.polling_hash == token_hash(secret),
            DeviceError::Invalid
        );
        if request.expires_at <= now {
            journal.device_pairings.remove(&id);
            self.save(&journal)?;
            return Ok(PairingPoll::Expired);
        }
        ensure!(
            request
                .last_poll
                .is_none_or(|at| now.saturating_sub(at) >= 2),
            DeviceError::RateLimited
        );
        request.last_poll = Some(now);
        if !request.approved {
            self.save(&journal)?;
            return Ok(PairingPoll::Pending);
        }
        let label = request.label.clone();
        let account = request.account;
        journal.devices.retain(|_, device| device.expires_at > now);
        ensure!(
            journal
                .devices
                .values()
                .filter(|device| device.account == account)
                .count()
                < DEVICES_PER_ACCOUNT
                && (account.is_none()
                    || journal
                        .devices
                        .values()
                        .filter(|device| device.account.is_some())
                        .count()
                        < MAX_GUEST_DEVICES),
            DeviceError::Capacity
        );
        let expires_at = now
            .checked_add(DEVICE_SECONDS)
            .ok_or(DeviceError::Invalid)?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let device_id = Uuid::new_v4();
        journal.devices.insert(
            device_id,
            StoredDevice {
                label,
                token_hash: token_hash(&token),
                expires_at,
                account,
            },
        );
        journal.device_pairings.remove(&id);
        self.save(&journal)?;
        Ok(PairingPoll::Authorized { device_id, token })
    }
    pub(crate) fn device_identity(
        &self,
        token: &str,
        now: i64,
    ) -> anyhow::Result<Option<DeviceIdentity>> {
        if !valid_token(token) {
            return Ok(None);
        }
        let hash = token_hash(token);
        Ok(self
            .read()?
            .devices
            .iter()
            .find(|(_, device)| device.token_hash == hash && device.expires_at > now)
            .map(|(id, device)| DeviceIdentity {
                device_id: *id,
                account: device.account,
            }))
    }
    #[cfg(test)]
    pub(crate) fn revoke_device(&self, id: Uuid, identity: &Identity) -> anyhow::Result<()> {
        self.revoke_device_as(id, |_| Ok(identity.clone()))
    }

    pub(crate) fn revoke_device_from_session(
        &self,
        id: Uuid,
        token: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.revoke_device_as(id, |journal| journal.device_browser_identity(token, now))
    }

    fn revoke_device_as(
        &self,
        id: Uuid,
        identity: impl FnOnce(&Journal) -> anyhow::Result<Identity>,
    ) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        let account = journal.device_account(&identity(&journal)?)?;
        ensure!(
            journal
                .devices
                .get(&id)
                .is_none_or(|device| device.account == account),
            DeviceError::Forbidden
        );
        journal.devices.remove(&id);
        self.save(&journal)
    }
    pub(crate) fn list_devices(
        &self,
        identity: &Identity,
        now: i64,
    ) -> anyhow::Result<Vec<DeviceView>> {
        let journal = self.read()?;
        let account = journal.device_account(identity)?;
        Ok(Self::device_views(&journal, account, now))
    }

    pub(crate) fn active_device_ids(
        &self,
        account: Option<u64>,
        now: i64,
    ) -> anyhow::Result<std::collections::HashSet<Uuid>> {
        let journal = self.read()?;
        ensure!(
            journal.device_account_active(account),
            DeviceError::Forbidden
        );
        Ok(journal
            .devices
            .iter()
            .filter(|(_, device)| device.account == account && device.expires_at > now)
            .map(|(id, _)| *id)
            .collect())
    }

    fn device_views(journal: &Journal, account: Option<u64>, now: i64) -> Vec<DeviceView> {
        journal
            .devices
            .iter()
            .filter(|(_, d)| d.expires_at > now && d.account == account)
            .map(|(id, d)| DeviceView {
                id: *id,
                label: d.label.clone(),
                expires_at: d.expires_at,
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "device_auth_tests.rs"]
mod device_auth_tests;
