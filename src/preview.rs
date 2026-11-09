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

/// The shortest gap between two fetches through the same grant. A page that
/// polls on a timer, or a `<script>` tag retrying in a loop, turns one
/// preview into a sustained tunnel otherwise — see `REMOTE_PREVIEW.md` T5's
/// "one connection per grant."
pub const MIN_FETCH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewGrant {
    id: String,
    device_fingerprint: String,
    target: TargetSession,
    port: u16,
    expires_at: SystemTime,
    last_fetch: Option<SystemTime>,
}

/// Why a fetch through a grant was refused, kept distinct from `validate`'s
/// plain `None` only where the caller needs to answer differently: a rate
/// limit is reported so the device can slow down, where "no such grant" is
/// not distinguished by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFetchError {
    /// Does not exist, has expired, belongs to another device, or the
    /// target or port is no longer configured for preview — these are not
    /// distinguished, for the same anti-enumeration reason as `validate`.
    NotFound,
    /// Fetched again before `MIN_FETCH_INTERVAL` has passed.
    TooSoon,
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
            last_fetch: None,
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

    /// Ends a grant, but only the one `device_fingerprint` itself holds.
    /// Returns whether a grant was actually ended, without distinguishing
    /// "no such grant" from "that grant belongs to another device" — the
    /// same anti-enumeration reasoning as `validate`.
    pub fn revoke_owned(&mut self, now: SystemTime, id: &str, device_fingerprint: &str) -> bool {
        self.purge_expired(now);
        let before = self.grants.len();
        self.grants
            .retain(|grant| !(grant.id == id && grant.device_fingerprint == device_fingerprint));
        self.grants.len() != before
    }

    /// Re-validates a grant immediately before it is used to fetch, and — if
    /// it is still good — records this instant as its last fetch. `allowed`
    /// is the caller's live recheck of the target and its `preview_ports`
    /// allowlist (this module only tracks presence and expiry; see the
    /// module doc comment).
    pub fn begin_fetch(
        &mut self,
        now: SystemTime,
        id: &str,
        device_fingerprint: &str,
        allowed: impl FnOnce(&TargetSession, u16) -> bool,
    ) -> Result<PreviewGrant, PreviewFetchError> {
        self.purge_expired(now);
        let grant = self
            .grants
            .iter_mut()
            .find(|grant| grant.id == id && grant.device_fingerprint == device_fingerprint)
            .ok_or(PreviewFetchError::NotFound)?;
        if !allowed(&grant.target, grant.port) {
            return Err(PreviewFetchError::NotFound);
        }
        if let Some(last) = grant.last_fetch
            && now.duration_since(last).unwrap_or(Duration::ZERO) < MIN_FETCH_INTERVAL
        {
            return Err(PreviewFetchError::TooSoon);
        }
        grant.last_fetch = Some(now);
        Ok(grant.clone())
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

    use super::{GRANT_LIFETIME, MIN_FETCH_INTERVAL, PreviewFetchError, PreviewGrants};
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
    fn revoking_a_grant_requires_owning_it() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let theirs = grants.grant(now, "device-b".to_owned(), target(), 3000);

        assert!(!grants.revoke_owned(now, theirs.id(), "device-a"));
        assert_eq!(
            grants.validate(now, theirs.id(), "device-b"),
            Some(theirs.clone())
        );

        assert!(grants.revoke_owned(now, theirs.id(), "device-b"));
        assert_eq!(grants.validate(now, theirs.id(), "device-b"), None);
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
    fn a_fetch_revalidates_against_the_live_allowlist_not_just_expiry() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let grant = grants.grant(now, "device-a".to_owned(), target(), 3000);

        assert_eq!(
            grants
                .begin_fetch(now, grant.id(), "device-a", |session, port| {
                    *session == target() && port == 3000
                })
                .map(|grant| grant.id().to_owned()),
            Ok(grant.id().to_owned())
        );

        // The port the caller's live recheck no longer allows is refused
        // even though the grant itself has not expired or been revoked.
        assert_eq!(
            grants.begin_fetch(now + MIN_FETCH_INTERVAL, grant.id(), "device-a", |_, _| {
                false
            }),
            Err(PreviewFetchError::NotFound)
        );
    }

    #[test]
    fn a_fetch_is_refused_before_the_minimum_interval_has_passed() {
        let now = SystemTime::UNIX_EPOCH;
        let mut grants = PreviewGrants::new();
        let grant = grants.grant(now, "device-a".to_owned(), target(), 3000);
        let allowed = |session: &TargetSession, port: u16| *session == target() && port == 3000;

        assert!(
            grants
                .begin_fetch(now, grant.id(), "device-a", allowed)
                .is_ok()
        );
        assert_eq!(
            grants.begin_fetch(
                now + MIN_FETCH_INTERVAL - Duration::from_millis(1),
                grant.id(),
                "device-a",
                allowed
            ),
            Err(PreviewFetchError::TooSoon)
        );
        assert!(
            grants
                .begin_fetch(now + MIN_FETCH_INTERVAL, grant.id(), "device-a", allowed)
                .is_ok()
        );
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
