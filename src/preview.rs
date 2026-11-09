//! Remote development preview grants.
//!
//! Required reading before touching this file: `REMOTE_PREVIEW.md`. A grant
//! is `(device, target, port, expiry)` and nothing else — no pane, no lease,
//! no subscription (T7) — so a device holding only a preview grant has no way
//! to reach any pane API. Grants are never persisted (T8): a daemon restart
//! drops them all by construction, and a revoked device, a removed target, or
//! a target whose `preview_ports` allowlist narrowed must end the affected
//! grants immediately. This module only tracks presence, expiry, and which
//! device a grant belongs to; the caller is responsible for re-checking device
//! admission and the live `preview_ports` allowlist on every fetch, not only
//! at grant time.

use std::time::{Duration, SystemTime};

use crate::model::TargetSession;
use crate::pairing;

/// Long enough to load a page and a handful of its own assets; short enough
/// that a forgotten grant does not outlive the reason it was made.
pub const GRANT_LIFETIME: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewGrant {
    id: String,
    device_fingerprint: String,
    target: TargetSession,
    port: u16,
    expires_at: SystemTime,
}

impl PreviewGrant {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn device_fingerprint(&self) -> &str {
        &self.device_fingerprint
    }

    pub fn target(&self) -> &TargetSession {
        &self.target
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }
}

/// In-memory grants for one running daemon. Never serialized, never written
/// to `Config`: an outstanding grant is a live capability, not a setting.
#[derive(Debug, Default)]
pub struct PreviewGrants {
    grants: Vec<PreviewGrant>,
}

impl PreviewGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a grant. The caller has already decided this device may preview
    /// this target and port — this only records that decision with an
    /// expiry, it does not make the decision.
    pub fn grant(
        &mut self,
        now: SystemTime,
        device_fingerprint: String,
        target: TargetSession,
        port: u16,
    ) -> PreviewGrant {
        self.purge_expired(now);
        let grant = PreviewGrant {
            id: grant_id(),
            device_fingerprint,
            target,
            port,
            expires_at: now + GRANT_LIFETIME,
        };
        self.grants.push(grant.clone());
        grant
    }

    /// Look up a grant by id, for a device presenting that id. `None` if it
    /// does not exist, has expired, or belongs to a different device — the
    /// three cases are not distinguished, so a probing guess learns nothing
    /// a wrong one would not.
    pub fn validate(
        &mut self,
        now: SystemTime,
        id: &str,
        device_fingerprint: &str,
    ) -> Option<PreviewGrant> {
        self.purge_expired(now);
        self.grants
            .iter()
            .find(|grant| grant.id == id && grant.device_fingerprint == device_fingerprint)
            .cloned()
    }

    pub fn revoke(&mut self, id: &str) {
        self.grants.retain(|grant| grant.id != id);
    }

    /// Ends every grant held by one device, e.g. when it is unpaired.
    pub fn revoke_device(&mut self, device_fingerprint: &str) {
        self.grants
            .retain(|grant| grant.device_fingerprint != device_fingerprint);
    }

    /// Ends every grant for a target that disappeared from configuration, or
    /// whose `preview_ports` no longer names the grant's port. `allowed`
    /// answers "is this (target, port) still configured?" against the
    /// configuration the caller just reloaded.
    pub fn retain_allowed(&mut self, mut allowed: impl FnMut(&TargetSession, u16) -> bool) {
        self.grants
            .retain(|grant| allowed(&grant.target, grant.port));
    }

    fn purge_expired(&mut self, now: SystemTime) {
        self.grants.retain(|grant| !grant.expired(now));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.grants.len()
    }
}

/// An unpredictable id, the same way a pairing token is: not because this
/// code relies on it alone — every fetch still re-checks the device and the
/// live configuration — but because a grant id appears in a URL path, and a
/// guessable one would be worth guessing.
fn grant_id() -> String {
    pairing::secret_bytes(16)
        .map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect())
        .unwrap_or_else(|_| format!("{:016x}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{GRANT_LIFETIME, PreviewGrants};
    use crate::model::TargetSession;

    fn target() -> TargetSession {
        TargetSession::new("host-a", "work")
    }

    #[test]
    fn a_grant_is_usable_only_by_the_device_it_was_issued_to() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let grant = grants.grant(now, "device-a".to_owned(), target(), 3000);

        assert_eq!(
            grants.validate(now, grant.id(), "device-a"),
            Some(grant.clone())
        );
        assert_eq!(grants.validate(now, grant.id(), "device-b"), None);
        assert_eq!(grants.validate(now, "not-an-id", "device-a"), None);
    }

    #[test]
    fn a_grant_expires_on_a_short_clock() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let grant = grants.grant(now, "device-a".to_owned(), target(), 3000);

        assert!(
            grants
                .validate(
                    now + GRANT_LIFETIME - Duration::from_secs(1),
                    grant.id(),
                    "device-a"
                )
                .is_some()
        );
        assert_eq!(
            grants.validate(now + GRANT_LIFETIME, grant.id(), "device-a"),
            None
        );
    }

    #[test]
    fn revoking_a_grant_ends_it_immediately() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let grant = grants.grant(now, "device-a".to_owned(), target(), 3000);

        grants.revoke(grant.id());

        assert_eq!(grants.validate(now, grant.id(), "device-a"), None);
    }

    #[test]
    fn revoking_a_device_ends_every_grant_it_holds_but_no_others() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let mine = grants.grant(now, "device-a".to_owned(), target(), 3000);
        let theirs = grants.grant(now, "device-b".to_owned(), target(), 3000);

        grants.revoke_device("device-a");

        assert_eq!(grants.validate(now, mine.id(), "device-a"), None);
        assert_eq!(grants.validate(now, theirs.id(), "device-b"), Some(theirs));
    }

    #[test]
    fn a_target_or_port_dropped_from_configuration_ends_its_grants() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let kept = grants.grant(now, "device-a".to_owned(), target(), 3000);
        let dropped_port = grants.grant(now, "device-a".to_owned(), target(), 4000);
        let other_target = TargetSession::new("host-b", "work");
        let dropped_target = grants.grant(now, "device-a".to_owned(), other_target.clone(), 3000);

        // host-a keeps only port 3000; host-b is gone entirely.
        grants.retain_allowed(|session, port| *session == target() && port == 3000);

        assert_eq!(grants.validate(now, kept.id(), "device-a"), Some(kept));
        assert_eq!(grants.validate(now, dropped_port.id(), "device-a"), None);
        assert_eq!(grants.validate(now, dropped_target.id(), "device-a"), None);
        let _ = other_target;
    }

    #[test]
    fn grant_ids_are_unpredictable() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let first = grants.grant(now, "device-a".to_owned(), target(), 3000);
        let second = grants.grant(now, "device-a".to_owned(), target(), 3000);

        assert_ne!(first.id(), second.id());
        assert_eq!(grants.len(), 2);
    }
}
