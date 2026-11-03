use super::*;

const PAIR_SECONDS: i64 = 300;
const DEVICE_SECONDS: i64 = 30 * 86400;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredDevice {
    label: String,
    token_hash: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredPairing {
    label: String,
    code_hash: String,
    polling_hash: String,
    expires_at: i64,
    approved: bool,
    last_poll: Option<i64>,
}

#[derive(Debug)]
pub(crate) enum DeviceError {
    Invalid,
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
        self.devices.len() <= 16
            && self.device_pairings.len() <= 32
            && self.device_approval_attempts.len() <= 5
            && self.devices.values().all(|device| {
                valid_label(&device.label)
                    && valid_token(&device.token_hash)
                    && device.expires_at > 0
            })
            && self.device_pairings.values().all(|request| {
                valid_label(&request.label)
                    && valid_token(&request.code_hash)
                    && valid_token(&request.polling_hash)
                    && request.expires_at > 0
            })
    }

    fn device_owner(&self, identity: &Identity) -> bool {
        match identity {
            Identity::Owner => true,
            Identity::Telegram(id) => self.owner_id == Some(*id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DeviceIdentity {
    pub device_id: Uuid,
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
    pub(crate) fn approve_device_pairing(
        &self,
        id: Uuid,
        code: &str,
        identity: &Identity,
        now: i64,
    ) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        ensure!(journal.device_owner(identity), DeviceError::Forbidden);
        journal
            .device_approval_attempts
            .retain(|at| now.saturating_sub(*at) < 60);
        ensure!(
            journal.device_approval_attempts.len() < 5,
            DeviceError::RateLimited
        );
        journal.device_approval_attempts.push_back(now);
        // Failed guesses consume the owner's budget too.
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
        journal.devices.retain(|_, device| device.expires_at > now);
        ensure!(journal.devices.len() < 16, DeviceError::Capacity);
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
            .map(|(id, _)| DeviceIdentity { device_id: *id }))
    }
    pub(crate) fn revoke_device(&self, id: Uuid) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut journal = self.read()?;
        journal.devices.remove(&id);
        self.save(&journal)
    }
    pub(crate) fn list_devices(
        &self,
        identity: &Identity,
        now: i64,
    ) -> anyhow::Result<Vec<DeviceView>> {
        let journal = self.read()?;
        ensure!(journal.device_owner(identity), DeviceError::Forbidden);
        Ok(journal
            .devices
            .iter()
            .filter(|(_, d)| d.expires_at > now)
            .map(|(id, d)| DeviceView {
                id: *id,
                label: d.label.clone(),
                expires_at: d.expires_at,
            })
            .collect())
    }
}

#[cfg(test)]
#[path = "device_auth_tests.rs"]
mod device_auth_tests;
