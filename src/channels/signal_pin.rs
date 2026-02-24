use crate::config::schema::SignalConfig;
use crate::security::SecretStore;
use anyhow::{Context, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const SIGNAL_PIN_STATE_FILE: &str = "signal-pin-state.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalPinLockReason {
    Expired,
    LockedOut,
    NotConfigured,
    CorruptState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalPinReminder {
    pub hours_remaining: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalPinGateStatus {
    Open { reminder: Option<SignalPinReminder> },
    Locked { reason: SignalPinLockReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalPinVerifyResult {
    Verified {
        next_verify_at: DateTime<Utc>,
    },
    Incorrect {
        remaining_attempts: u32,
        lockout_minutes: u32,
    },
    LockedOut {
        remaining_minutes: u32,
    },
    NotConfigured,
    CorruptState,
}

#[derive(Debug, Clone)]
pub struct SignalPinStatus {
    pub verification_enabled: bool,
    pub state_path: PathBuf,
    pub configured: bool,
    pub locked: bool,
    pub lock_reason: Option<SignalPinLockReason>,
    pub lockout_remaining_minutes: Option<u32>,
    pub last_verified_at: Option<DateTime<Utc>>,
    pub next_verify_at: Option<DateTime<Utc>>,
    pub verification_count: u32,
    pub failed_attempts: u32,
}

#[derive(Debug, Clone)]
pub struct SignalPinManager {
    state_path: PathBuf,
    store: SecretStore,
    policy: SignalPinPolicy,
}

#[derive(Debug, Clone)]
struct SignalPinPolicy {
    pin_verification: bool,
    pin_reverify_days: u32,
    pin_adaptive_schedule: bool,
    pin_reminder_hours_before: u32,
    pin_max_failed_attempts: u32,
    pin_lockout_minutes: u32,
    pin_min_length: usize,
    pin_allow_alphanumeric: bool,
}

#[derive(Debug, Clone)]
struct SignalPinState {
    pin_hash: String,
    last_verified_at: Option<DateTime<Utc>>,
    verification_count: u32,
    next_verify_at: Option<DateTime<Utc>>,
    failed_attempts: u32,
    locked_until: Option<DateTime<Utc>>,
    last_reminder_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSignalPinState {
    pin_hash: String,
    #[serde(default)]
    last_verified_at: Option<String>,
    #[serde(default)]
    verification_count: u32,
    #[serde(default)]
    next_verify_at: Option<String>,
    #[serde(default)]
    failed_attempts: u32,
    #[serde(default)]
    locked_until: Option<String>,
    #[serde(default)]
    last_reminder_at: Option<String>,
}

enum LoadedState {
    Missing,
    Corrupt,
    Available(SignalPinState),
}

impl SignalPinManager {
    pub fn new(config: &SignalConfig, zeroclaw_dir: &Path, secrets_encrypt: bool) -> Self {
        Self {
            state_path: zeroclaw_dir.join(SIGNAL_PIN_STATE_FILE),
            store: SecretStore::new(zeroclaw_dir, secrets_encrypt),
            policy: SignalPinPolicy::from_config(config),
        }
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn set_pin(&self, pin: &str) -> Result<()> {
        self.set_pin_at(pin, Utc::now())
    }

    pub fn verify_pin_attempt(&self, pin: &str) -> Result<SignalPinVerifyResult> {
        self.verify_pin_attempt_at(pin, Utc::now())
    }

    pub fn verify_pin_without_side_effects(&self, pin: &str) -> Result<bool> {
        match self.load_state()? {
            LoadedState::Available(state) => Ok(verify_pin_against_hash(pin, &state.pin_hash)),
            LoadedState::Missing | LoadedState::Corrupt => Ok(false),
        }
    }

    pub fn gate_status(&self) -> Result<SignalPinGateStatus> {
        self.gate_status_at(Utc::now())
    }

    pub fn mark_reminder_sent(&self) -> Result<()> {
        self.mark_reminder_sent_at(Utc::now())
    }

    pub fn status(&self) -> Result<SignalPinStatus> {
        self.status_at(Utc::now())
    }

    fn set_pin_at(&self, pin: &str, now: DateTime<Utc>) -> Result<()> {
        validate_pin(pin, &self.policy)?;

        let next_interval_days = self.next_interval_days_after_count(0);
        let state = SignalPinState {
            pin_hash: hash_pin(pin)?,
            last_verified_at: Some(now),
            verification_count: 0,
            next_verify_at: Some(now + Duration::days(i64::from(next_interval_days))),
            failed_attempts: 0,
            locked_until: None,
            last_reminder_at: None,
        };
        self.save_state(&state)
    }

    fn verify_pin_attempt_at(
        &self,
        pin: &str,
        now: DateTime<Utc>,
    ) -> Result<SignalPinVerifyResult> {
        let loaded = self.load_state()?;

        let mut state = match loaded {
            LoadedState::Missing => return Ok(SignalPinVerifyResult::NotConfigured),
            LoadedState::Corrupt => return Ok(SignalPinVerifyResult::CorruptState),
            LoadedState::Available(state) => state,
        };

        if state.pin_hash.trim().is_empty() {
            return Ok(SignalPinVerifyResult::NotConfigured);
        }

        if let Some(locked_until) = state.locked_until {
            if locked_until > now {
                return Ok(SignalPinVerifyResult::LockedOut {
                    remaining_minutes: minutes_until(now, locked_until),
                });
            }
        }

        if verify_pin_against_hash(pin, &state.pin_hash) {
            state.failed_attempts = 0;
            state.locked_until = None;
            state.verification_count = state.verification_count.saturating_add(1);
            state.last_verified_at = Some(now);
            state.next_verify_at = Some(
                now + Duration::days(i64::from(
                    self.next_interval_days_after_count(state.verification_count),
                )),
            );
            state.last_reminder_at = None;
            let next_verify_at = state.next_verify_at;
            self.save_state(&state)?;
            return Ok(SignalPinVerifyResult::Verified {
                next_verify_at: next_verify_at.unwrap_or(now),
            });
        }

        state.failed_attempts = state.failed_attempts.saturating_add(1);
        if state.failed_attempts >= self.policy.pin_max_failed_attempts {
            state.failed_attempts = self.policy.pin_max_failed_attempts;
            state.locked_until =
                Some(now + Duration::minutes(i64::from(self.policy.pin_lockout_minutes)));
            self.save_state(&state)?;
            return Ok(SignalPinVerifyResult::LockedOut {
                remaining_minutes: self.policy.pin_lockout_minutes,
            });
        }

        self.save_state(&state)?;
        Ok(SignalPinVerifyResult::Incorrect {
            remaining_attempts: self
                .policy
                .pin_max_failed_attempts
                .saturating_sub(state.failed_attempts),
            lockout_minutes: self.policy.pin_lockout_minutes,
        })
    }

    fn gate_status_at(&self, now: DateTime<Utc>) -> Result<SignalPinGateStatus> {
        if !self.policy.pin_verification {
            return Ok(SignalPinGateStatus::Open { reminder: None });
        }

        let state = match self.load_state()? {
            LoadedState::Missing => {
                return Ok(SignalPinGateStatus::Locked {
                    reason: SignalPinLockReason::NotConfigured,
                });
            }
            LoadedState::Corrupt => {
                return Ok(SignalPinGateStatus::Locked {
                    reason: SignalPinLockReason::CorruptState,
                });
            }
            LoadedState::Available(state) => state,
        };

        if state.pin_hash.trim().is_empty() {
            return Ok(SignalPinGateStatus::Locked {
                reason: SignalPinLockReason::NotConfigured,
            });
        }

        if let Some(locked_until) = state.locked_until {
            if locked_until > now {
                return Ok(SignalPinGateStatus::Locked {
                    reason: SignalPinLockReason::LockedOut,
                });
            }
        }

        let Some(next_verify_at) = state.next_verify_at else {
            return Ok(SignalPinGateStatus::Locked {
                reason: SignalPinLockReason::NotConfigured,
            });
        };

        if next_verify_at <= now {
            return Ok(SignalPinGateStatus::Locked {
                reason: SignalPinLockReason::Expired,
            });
        }

        let reminder = self
            .reminder_hours_remaining(&state, now)
            .map(|hours_remaining| SignalPinReminder { hours_remaining });

        Ok(SignalPinGateStatus::Open { reminder })
    }

    fn mark_reminder_sent_at(&self, now: DateTime<Utc>) -> Result<()> {
        let loaded = self.load_state()?;
        let mut state = match loaded {
            LoadedState::Available(state) => state,
            LoadedState::Missing | LoadedState::Corrupt => return Ok(()),
        };

        state.last_reminder_at = Some(now);
        self.save_state(&state)
    }

    fn status_at(&self, now: DateTime<Utc>) -> Result<SignalPinStatus> {
        let loaded = self.load_state()?;

        let mut status = SignalPinStatus {
            verification_enabled: self.policy.pin_verification,
            state_path: self.state_path.clone(),
            configured: false,
            locked: self.policy.pin_verification,
            lock_reason: if self.policy.pin_verification {
                Some(SignalPinLockReason::NotConfigured)
            } else {
                None
            },
            lockout_remaining_minutes: None,
            last_verified_at: None,
            next_verify_at: None,
            verification_count: 0,
            failed_attempts: 0,
        };

        match loaded {
            LoadedState::Missing => {
                if !self.policy.pin_verification {
                    status.locked = false;
                    status.lock_reason = None;
                }
            }
            LoadedState::Corrupt => {
                status.lock_reason = Some(SignalPinLockReason::CorruptState);
            }
            LoadedState::Available(state) => {
                status.configured = !state.pin_hash.trim().is_empty();
                status.last_verified_at = state.last_verified_at;
                status.next_verify_at = state.next_verify_at;
                status.verification_count = state.verification_count;
                status.failed_attempts = state.failed_attempts;

                if !self.policy.pin_verification {
                    status.locked = false;
                    status.lock_reason = None;
                    return Ok(status);
                }

                if let Some(locked_until) = state.locked_until {
                    if locked_until > now {
                        status.locked = true;
                        status.lock_reason = Some(SignalPinLockReason::LockedOut);
                        status.lockout_remaining_minutes = Some(minutes_until(now, locked_until));
                        return Ok(status);
                    }
                }

                if let Some(next_verify_at) = state.next_verify_at {
                    if next_verify_at <= now {
                        status.locked = true;
                        status.lock_reason = Some(SignalPinLockReason::Expired);
                    } else {
                        status.locked = false;
                        status.lock_reason = None;
                    }
                } else {
                    status.locked = true;
                    status.lock_reason = Some(SignalPinLockReason::NotConfigured);
                }
            }
        }

        Ok(status)
    }

    fn reminder_hours_remaining(&self, state: &SignalPinState, now: DateTime<Utc>) -> Option<u64> {
        if self.policy.pin_reminder_hours_before == 0 {
            return None;
        }

        let next_verify_at = state.next_verify_at?;
        if next_verify_at <= now {
            return None;
        }

        let reminder_window = Duration::hours(i64::from(self.policy.pin_reminder_hours_before));
        let reminder_start = next_verify_at - reminder_window;
        if now < reminder_start {
            return None;
        }

        if state
            .last_reminder_at
            .is_some_and(|last| last >= reminder_start)
        {
            return None;
        }

        let secs_remaining = (next_verify_at - now).num_seconds().max(0) as u64;
        let hours = (secs_remaining + 3599) / 3600;
        Some(hours.max(1))
    }

    fn next_interval_days_after_count(&self, verification_count: u32) -> u32 {
        if !self.policy.pin_adaptive_schedule {
            return self.policy.pin_reverify_days;
        }

        match verification_count {
            0 => 1,
            1 => 3,
            2 => 7,
            _ => self.policy.pin_reverify_days,
        }
    }

    fn load_state(&self) -> Result<LoadedState> {
        if !self.state_path.exists() {
            return Ok(LoadedState::Missing);
        }

        let raw = match fs::read_to_string(&self.state_path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Failed to read Signal PIN state file; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };

        let persisted: PersistedSignalPinState = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Failed to parse Signal PIN state file; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };

        let pin_hash = match self.store.decrypt(persisted.pin_hash.trim()) {
            Ok(pin_hash) => pin_hash,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Failed to decrypt Signal PIN hash; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };

        let last_verified_at = match parse_optional_timestamp(persisted.last_verified_at.as_deref())
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Invalid Signal PIN last_verified_at timestamp; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };
        let next_verify_at = match parse_optional_timestamp(persisted.next_verify_at.as_deref()) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Invalid Signal PIN next_verify_at timestamp; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };
        let locked_until = match parse_optional_timestamp(persisted.locked_until.as_deref()) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Invalid Signal PIN locked_until timestamp; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };
        let last_reminder_at = match parse_optional_timestamp(persisted.last_reminder_at.as_deref())
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %self.state_path.display(),
                    "Invalid Signal PIN last_reminder_at timestamp; treating as locked (fail-closed): {error}"
                );
                return Ok(LoadedState::Corrupt);
            }
        };

        let state = SignalPinState {
            pin_hash,
            last_verified_at,
            verification_count: persisted.verification_count,
            next_verify_at,
            failed_attempts: persisted.failed_attempts,
            locked_until,
            last_reminder_at,
        };

        Ok(LoadedState::Available(state))
    }

    fn save_state(&self, state: &SignalPinState) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create Signal PIN state directory {}",
                    parent.display()
                )
            })?;
        }

        let encrypted_hash = self
            .store
            .encrypt(&state.pin_hash)
            .context("Failed to encrypt Signal PIN hash")?;

        let persisted = PersistedSignalPinState {
            pin_hash: encrypted_hash,
            last_verified_at: state.last_verified_at.map(|value| value.to_rfc3339()),
            verification_count: state.verification_count,
            next_verify_at: state.next_verify_at.map(|value| value.to_rfc3339()),
            failed_attempts: state.failed_attempts,
            locked_until: state.locked_until.map(|value| value.to_rfc3339()),
            last_reminder_at: state.last_reminder_at.map(|value| value.to_rfc3339()),
        };

        let body = serde_json::to_string_pretty(&persisted)
            .context("Failed to serialize Signal PIN state file")?;

        let temp_path = self
            .state_path
            .with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        fs::write(&temp_path, body).with_context(|| {
            format!(
                "Failed to write temporary Signal PIN state file {}",
                temp_path.display()
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600));
        }

        fs::rename(&temp_path, &self.state_path).with_context(|| {
            format!(
                "Failed to atomically replace Signal PIN state file {}",
                self.state_path.display()
            )
        })?;

        Ok(())
    }
}

impl SignalPinPolicy {
    fn from_config(config: &SignalConfig) -> Self {
        Self {
            pin_verification: config.pin_verification,
            pin_reverify_days: config.pin_reverify_days.clamp(1, 30),
            pin_adaptive_schedule: config.pin_adaptive_schedule,
            pin_reminder_hours_before: config.pin_reminder_hours_before,
            pin_max_failed_attempts: config.pin_max_failed_attempts.max(1),
            pin_lockout_minutes: config.pin_lockout_minutes.max(1),
            pin_min_length: config.pin_min_length.max(1) as usize,
            pin_allow_alphanumeric: config.pin_allow_alphanumeric,
        }
    }
}

fn parse_optional_timestamp(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value else {
        return Ok(None);
    };

    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let parsed = DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("Invalid RFC3339 timestamp in Signal PIN state: {value}"))?;
    Ok(Some(parsed.with_timezone(&Utc)))
}

fn hash_pin(pin: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hashed = Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!("Failed to hash Signal PIN with Argon2id: {error}"))?;
    Ok(hashed.to_string())
}

fn verify_pin_against_hash(pin: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };

    Argon2::default()
        .verify_password(pin.as_bytes(), &parsed_hash)
        .is_ok()
}

fn validate_pin(pin: &str, policy: &SignalPinPolicy) -> Result<()> {
    let normalized = pin.trim();
    if normalized.len() < policy.pin_min_length {
        anyhow::bail!(
            "Signal PIN must be at least {} characters long",
            policy.pin_min_length
        );
    }

    if policy.pin_allow_alphanumeric {
        if !normalized.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            anyhow::bail!(
                "Signal PIN may contain only ASCII letters and digits when pin_allow_alphanumeric=true"
            );
        }
    } else if !normalized.chars().all(|ch| ch.is_ascii_digit()) {
        anyhow::bail!("Signal PIN must contain only digits when pin_allow_alphanumeric=false");
    }

    Ok(())
}

fn minutes_until(now: DateTime<Utc>, target: DateTime<Utc>) -> u32 {
    let secs = (target - now).num_seconds().max(0) as u64;
    ((secs + 59) / 60) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_config() -> SignalConfig {
        SignalConfig {
            http_url: "http://127.0.0.1:8686".to_string(),
            account: "+1234567890".to_string(),
            group_id: None,
            allowed_from: vec!["*".to_string()],
            ignore_attachments: false,
            ignore_stories: false,
            pin_verification: true,
            pin_reverify_days: 7,
            pin_adaptive_schedule: true,
            pin_reminder_hours_before: 12,
            pin_max_failed_attempts: 5,
            pin_lockout_minutes: 30,
            pin_min_length: 4,
            pin_allow_alphanumeric: true,
        }
    }

    fn setup_manager(config: &SignalConfig) -> (tempfile::TempDir, SignalPinManager) {
        let dir = tempdir().unwrap();
        let manager = SignalPinManager::new(config, dir.path(), true);
        (dir, manager)
    }

    #[test]
    fn set_pin_persists_encrypted_hash_and_initial_schedule() {
        let (dir, manager) = setup_manager(&make_config());
        let now = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        manager.set_pin_at("Abc123", now).unwrap();

        let raw = fs::read_to_string(dir.path().join(SIGNAL_PIN_STATE_FILE)).unwrap();
        assert!(raw.contains("\"pin_hash\""));
        assert!(raw.contains("enc2:"));

        let status = manager.status_at(now).unwrap();
        assert!(status.configured);
        assert!(!status.locked);
        assert_eq!(status.verification_count, 0);
        assert_eq!(
            status.next_verify_at,
            Some(
                DateTime::parse_from_rfc3339("2026-02-24T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn verify_pin_advances_adaptive_schedule() {
        let (_dir, manager) = setup_manager(&make_config());
        let t0 = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        manager.set_pin_at("Abc123", t0).unwrap();

        let t1 = DateTime::parse_from_rfc3339("2026-02-24T13:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let v1 = manager.verify_pin_attempt_at("Abc123", t1).unwrap();
        match v1 {
            SignalPinVerifyResult::Verified { next_verify_at } => {
                assert_eq!(
                    next_verify_at,
                    DateTime::parse_from_rfc3339("2026-02-27T13:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                );
            }
            other => panic!("expected verified, got {other:?}"),
        }

        let t2 = DateTime::parse_from_rfc3339("2026-02-27T13:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let v2 = manager.verify_pin_attempt_at("Abc123", t2).unwrap();
        match v2 {
            SignalPinVerifyResult::Verified { next_verify_at } => {
                assert_eq!(
                    next_verify_at,
                    DateTime::parse_from_rfc3339("2026-03-06T13:30:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                );
            }
            other => panic!("expected verified, got {other:?}"),
        }
    }

    #[test]
    fn fixed_schedule_respects_pin_reverify_days() {
        let mut config = make_config();
        config.pin_adaptive_schedule = false;
        config.pin_reverify_days = 10;
        let (_dir, manager) = setup_manager(&config);

        let now = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        manager.set_pin_at("1234", now).unwrap();
        let status = manager.status_at(now).unwrap();
        assert_eq!(
            status.next_verify_at,
            Some(
                DateTime::parse_from_rfc3339("2026-03-05T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn incorrect_attempts_trigger_lockout() {
        let mut config = make_config();
        config.pin_max_failed_attempts = 2;
        config.pin_lockout_minutes = 15;
        let (_dir, manager) = setup_manager(&config);

        let now = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        manager.set_pin_at("9999", now).unwrap();

        let first = manager.verify_pin_attempt_at("0000", now).unwrap();
        assert_eq!(
            first,
            SignalPinVerifyResult::Incorrect {
                remaining_attempts: 1,
                lockout_minutes: 15
            }
        );

        let second = manager
            .verify_pin_attempt_at("1111", now + Duration::seconds(1))
            .unwrap();
        assert_eq!(
            second,
            SignalPinVerifyResult::LockedOut {
                remaining_minutes: 15
            }
        );

        let status = manager.status_at(now + Duration::seconds(1)).unwrap();
        assert!(status.locked);
        assert_eq!(status.lock_reason, Some(SignalPinLockReason::LockedOut));
    }

    #[test]
    fn gate_is_fail_closed_when_state_missing_or_corrupt() {
        let (dir, manager) = setup_manager(&make_config());
        let status = manager.gate_status().unwrap();
        assert_eq!(
            status,
            SignalPinGateStatus::Locked {
                reason: SignalPinLockReason::NotConfigured
            }
        );

        fs::write(dir.path().join(SIGNAL_PIN_STATE_FILE), "not json").unwrap();
        let status = manager.gate_status().unwrap();
        assert_eq!(
            status,
            SignalPinGateStatus::Locked {
                reason: SignalPinLockReason::CorruptState
            }
        );
    }

    #[test]
    fn reminder_is_emitted_once_per_window() {
        let (_dir, manager) = setup_manager(&make_config());
        let now = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        manager.set_pin_at("A1b2", now).unwrap();

        let near_expiry = DateTime::parse_from_rfc3339("2026-02-24T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let first = manager.gate_status_at(near_expiry).unwrap();
        match first {
            SignalPinGateStatus::Open {
                reminder: Some(SignalPinReminder { hours_remaining }),
            } => assert_eq!(hours_remaining, 11),
            other => panic!("expected reminder, got {other:?}"),
        }

        manager.mark_reminder_sent_at(near_expiry).unwrap();
        let second = manager.gate_status_at(near_expiry).unwrap();
        assert_eq!(second, SignalPinGateStatus::Open { reminder: None });
    }

    #[test]
    fn verify_pin_without_side_effects_does_not_increment_counters() {
        let (_dir, manager) = setup_manager(&make_config());
        let now = DateTime::parse_from_rfc3339("2026-02-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        manager.set_pin_at("Abc123", now).unwrap();

        assert!(manager.verify_pin_without_side_effects("Abc123").unwrap());

        let status = manager.status_at(now).unwrap();
        assert_eq!(status.verification_count, 0);
        assert_eq!(status.failed_attempts, 0);
    }
}
