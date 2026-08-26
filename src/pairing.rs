//! Device pairing: what lets a client that is not on this machine connect.
//!
//! A paired device holds a secret; the daemon holds only its digest. That way
//! the configuration file is not itself a set of credentials — losing a backup
//! of it does not hand anyone a working device — and revoking one is deleting a
//! line rather than rotating everything.
//!
//! A token authenticates a device. It does not encrypt anything, and nothing
//! here pretends otherwise: confidentiality is the network's job, which is why
//! the daemon refuses to bind an address that is not loopback or a private
//! range. On a mesh like WireGuard or Tailscale that is already true; on the
//! open internet it would not be, and the way to reach the daemon from there is
//! the TLS public bridge or an operator-managed proxy rather than a public
//! listener.

use std::fs::File;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// A pairing code is read off one screen and typed into another, so it avoids
/// the characters people confuse when doing that.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_CHARACTERS: usize = super_herdr_bridge::PAIRING_CODE_CHARACTERS;
const TOKEN_BYTES: usize = 32;

/// How long a pairing code stays usable. Long enough to walk to another device,
/// short enough that one left on a screen is not a standing invitation.
pub const CODE_LIFETIME: Duration = super_herdr_bridge::PAIRING_CODE_LIFETIME;

/// Random bytes from the kernel.
///
/// `/dev/urandom` rather than a crate: this is a Unix-only daemon, the file is
/// the documented interface, and a dependency whose only job is to open it
/// would be a supply chain for no benefit.
fn secret_bytes(count: usize) -> Result<Vec<u8>> {
    let mut file = File::open("/dev/urandom").context("failed to open /dev/urandom")?;
    let mut bytes = vec![0_u8; count];
    file.read_exact(&mut bytes)
        .context("failed to read from /dev/urandom")?;
    Ok(bytes)
}

/// A device's secret. Held by the device; the daemon keeps only its digest.
pub fn token() -> Result<String> {
    Ok(hex(&secret_bytes(TOKEN_BYTES)?))
}

/// A short code a person can read aloud or type.
///
/// The alphabet is a power of two, so bytes map onto it without the modulo bias
/// that would make some codes likelier than others.
pub fn pairing_code() -> Result<String> {
    let bytes = secret_bytes(CODE_CHARACTERS)?;
    let code = bytes
        .iter()
        .map(|byte| CODE_ALPHABET[usize::from(byte % 32)] as char)
        .collect::<String>();
    Ok(format!("{}-{}", &code[..4], &code[4..]))
}

/// Accept a code however it was typed: case and separators are presentation.
pub fn normalize_code(entered: &str) -> String {
    super_herdr_bridge::normalize_pairing_code(entered)
}

pub fn fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex(&hasher.finalize())
}

/// Check a URL a pairing code can be offered at, and hand back its base.
///
/// Refused at the flag rather than at the screen, because everything after this
/// point produces something that looks right. A QR of an unreachable address
/// scans perfectly and fails at the far end, where the person holding the phone
/// cannot tell a wrong address from a bad camera, poor light, or a daemon that
/// is not running. A refusal here names the problem while somebody is still
/// looking at a terminal.
///
/// What is checked is what this can know: a scheme it makes sense to open, a
/// host that is not empty, and no fragment. Device-login codes are typed into
/// the page and never belong in a URL. Whether the host resolves and whether a
/// certificate validates are not knowable from here and are not pretended at.
pub fn pairing_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    let (scheme, rest) = trimmed
        .split_once("://")
        .context("a pairing URL needs a scheme, as in https://host.example:8790")?;
    if !matches!(scheme, "http" | "https") {
        bail!("a pairing URL must be http or https; {scheme:?} is neither");
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        bail!("a pairing URL needs a host");
    }
    // A token authenticates a device and encrypts nothing, so the code must not
    // cross a network that provides no confidentiality of its own.
    //
    // The rule used to be loopback or nothing, which refused the address of the
    // very listener this daemon is willing to bind. That protected nobody: the
    // browser has to send the code to the same host over the same plain HTTP to
    // pair at all, so refusing to make a QR of it left the code on the wire and
    // took the QR away. Whatever the listener will bind, a code may name.
    if scheme == "http" && !reachable_privately(host) {
        bail!("a pairing URL over http would carry the code in clear; use https");
    }
    if trimmed.contains('#') {
        bail!("a pairing URL cannot carry a fragment; device-login codes stay out of URLs");
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

/// Whether a URL's host is somewhere a private network already protects.
///
/// Deliberately the same question [`crate::daemon::web::bindable`] answers, and
/// for the same reason — one of them saying yes while the other says no is how
/// a daemon ends up serving an address it will not name.
fn reachable_privately(host: &str) -> bool {
    let host = host.split('@').next_back().unwrap_or(host);
    // `[::1]:8790` and `192.168.1.4:8790` both carry a port; only the bracketed
    // form can contain a colon in the host itself.
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    if name.eq_ignore_ascii_case("localhost") || name.to_ascii_lowercase().ends_with(".local") {
        return true;
    }
    name.parse::<std::net::IpAddr>()
        .is_ok_and(crate::daemon::web::bindable)
}

/// Compare without revealing where two values first differ.
///
/// These are digests rather than secrets, so a timing leak would be a poor
/// oracle, but a comparison that stops early is the wrong habit to keep in the
/// one place that decides whether a device is allowed in.
pub fn matches(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// One outstanding pairing code, held only in daemon memory.
///
/// It is never written down: a code that survived a restart would be a
/// credential the operator did not know they still had.
/// How many wrong entries a code survives.
///
/// A code is one of 32^8 possibilities, so this is not what stops a guess — it
/// stops a flood, and leaves room for the typo that is the far likelier reason
/// an entry is wrong. Consuming the code on the first mistake would mean
/// fetching a new one every time somebody's thumb slipped.
pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone)]
pub struct PendingPairing {
    code: String,
    expires_at: SystemTime,
    attempts: u32,
}

impl PendingPairing {
    pub fn new(now: SystemTime) -> Result<Self> {
        Ok(Self {
            code: pairing_code()?,
            expires_at: now + CODE_LIFETIME,
            attempts: 0,
        })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }

    pub fn expired(&self, now: SystemTime) -> bool {
        now >= self.expires_at
    }

    /// Whether an entered code matches this one and is still live.
    pub fn accepts(&self, entered: &str, now: SystemTime) -> bool {
        !self.expired(now) && matches(&normalize_code(entered), &normalize_code(&self.code))
    }

    /// Record a wrong entry, and report whether the code is now spent.
    ///
    /// A code is good for one device, so the caller drops it after a match —
    /// and drops it here too once wrong entries stop looking like typing.
    pub fn record_failure(&mut self) -> bool {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts >= MAX_ATTEMPTS
    }

    pub fn attempts_remaining(&self) -> u32 {
        MAX_ATTEMPTS.saturating_sub(self.attempts)
    }
}

pub fn now_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{
        CODE_ALPHABET, CODE_LIFETIME, PendingPairing, fingerprint, matches, normalize_code,
        pairing_code, token,
    };

    #[test]
    fn a_pairing_url_is_refused_while_somebody_is_still_looking_at_a_terminal() {
        // The whole point of checking here: everything downstream of this
        // produces something that looks right. A QR of an unreachable address
        // scans perfectly and fails where nobody can diagnose it.
        for url in [
            "https://onio-ws01.tail15b0b2.ts.net:8790",
            "https://host.example",
            // Loopback over http is the one plain case that is not on a wire.
            "http://127.0.0.1:8790",
            "http://localhost:8790",
            // And the addresses the listener will actually bind. Refusing
            // these protected nothing: pairing sends the code to the same host
            // over the same plain HTTP whether it was scanned or typed.
            "http://192.168.1.42:8790",
            "http://10.0.0.4:8790",
            "http://100.101.102.103:8790",
            "http://[fd00::1]:8790",
            "http://vemunds-macbook-pro.local:8790",
        ] {
            assert!(super::pairing_url(url).is_ok(), "{url} should be accepted");
        }

        for (url, expected) in [
            ("host.example:8790", "scheme"),
            ("ftp://host.example", "http or https"),
            // A token authenticates and encrypts nothing, so plain http across
            // a network nobody controls would put the code on the wire. This
            // is the line that moved: private yes, public still no.
            ("http://host.example:8790", "clear"),
            ("http://93.184.216.34:8790", "clear"),
            ("http://8.8.8.8", "clear"),
            ("https://", "host"),
            // Device-login URLs are stable bases; codes never belong in them.
            ("https://host.example/#already", "fragment"),
        ] {
            let error = super::pairing_url(url)
                .expect_err(&format!("{url} should be refused"))
                .to_string();
            assert!(error.contains(expected), "{url}: {error}");
        }

        // A trailing slash would otherwise become a double slash before the
        // stable device-login URL.
        assert_eq!(
            super::pairing_url("https://host.example:8790/").unwrap(),
            "https://host.example:8790"
        );
    }

    #[test]
    fn a_token_is_unpredictable_and_stored_only_as_a_digest() {
        let first = token().expect("the kernel provides randomness");
        let second = token().expect("the kernel provides randomness");

        assert_ne!(first, second);
        assert_eq!(first.len(), 64, "32 bytes as hex");
        // What the configuration holds cannot be replayed as a token.
        assert_ne!(fingerprint(&first), first);
        assert_eq!(fingerprint(&first), fingerprint(&first));
        assert_ne!(fingerprint(&first), fingerprint(&second));
    }

    #[test]
    fn a_pairing_code_avoids_the_characters_people_mistype() {
        let code = pairing_code().expect("the kernel provides randomness");

        assert_eq!(code.len(), 9, "eight characters and one separator");
        for character in code.chars().filter(|character| *character != '-') {
            assert!(
                CODE_ALPHABET.contains(&(character as u8)),
                "{character} is not in the alphabet"
            );
        }
        for confusable in ['O', '0', 'I', '1'] {
            assert!(!code.contains(confusable), "{code} contains {confusable}");
        }
    }

    #[test]
    fn a_code_is_accepted_however_it_was_typed() {
        let now = SystemTime::UNIX_EPOCH;
        let pending = PendingPairing::new(now).expect("a code");
        let code = pending.code().to_owned();

        assert!(pending.accepts(&code, now));
        assert!(pending.accepts(&code.to_lowercase(), now));
        assert!(pending.accepts(&code.replace('-', ""), now));
        assert!(pending.accepts(&format!("  {code}  "), now));
        assert!(!pending.accepts("WRONGCODE", now));
    }

    #[test]
    fn a_code_stops_working_when_it_expires() {
        let now = SystemTime::UNIX_EPOCH;
        let pending = PendingPairing::new(now).expect("a code");
        let code = pending.code().to_owned();

        assert!(pending.accepts(&code, now + CODE_LIFETIME - Duration::from_secs(1)));
        assert!(!pending.accepts(&code, now + CODE_LIFETIME));
        assert!(pending.expired(now + CODE_LIFETIME));
    }

    #[test]
    fn a_code_survives_a_typo_but_not_a_flood() {
        let now = SystemTime::UNIX_EPOCH;
        let mut pending = PendingPairing::new(now).expect("a code");
        let code = pending.code().to_owned();

        // Four wrong entries, and the code somebody is squinting at still works.
        for expected in (1..super::MAX_ATTEMPTS).rev() {
            assert!(!pending.record_failure(), "spent too early");
            assert_eq!(pending.attempts_remaining(), expected);
            assert!(pending.accepts(&code, now));
        }
        // The fifth spends it, and the right code no longer helps.
        assert!(pending.record_failure());
        assert_eq!(pending.attempts_remaining(), 0);
    }

    #[test]
    fn comparison_does_not_stop_at_the_first_difference() {
        assert!(matches("abcd", "abcd"));
        assert!(!matches("abcd", "abce"));
        assert!(!matches("abcd", "abcde"));
        assert!(!matches("", "a"));
        assert!(matches("", ""));
    }

    #[test]
    fn normalizing_keeps_only_what_a_code_is_made_of() {
        assert_eq!(normalize_code("abcd-efgh"), "ABCDEFGH");
        assert_eq!(normalize_code("AB CD/EF\nGH"), "ABCDEFGH");
        // A long entry cannot smuggle extra characters past the comparison.
        assert_eq!(normalize_code(&"A".repeat(50)).len(), 8);
    }
}
