//! Web Push: waking a paired device whose browser is closed.
//!
//! Every notification path before this one needs a frontend awake to receive
//! it — a desktop process to run `notify-send`, or a browser tab holding an
//! open event stream. Web Push is the one sink that can reach a device with
//! no frontend running at all, by handing a push service (chosen by the
//! browser, outside this daemon's control) a subscription to wake a service
//! worker.
//!
//! That push service is a new, genuinely different trust boundary: it is a
//! third party that relays something the moment this daemon decides a device
//! should be woken. The design here keeps it capable of exactly that and
//! nothing more. A push message carries zero bytes of payload — not an
//! encrypted empty payload, an actually empty one — so the push service
//! learns only that a wake happened, never for which agent, pane, or kind of
//! event. The woken service worker then asks this daemon directly, over the
//! same authenticated connection every other request uses, what actually
//! happened.
//!
//! VAPID identifies this daemon to the push service with a signed token
//! (RFC 8292), not the device's own identity: the keypair is generated once
//! per installation and is never specific to a person, a device, or an agent.
//! It must survive every restart unchanged — a subscription is bound to the
//! public key the browser subscribed with, so losing or rotating the private
//! key silently breaks every paired device's push without telling anyone.
//! That is why loading it distinguishes "no key yet" from "a key that could
//! not be read": only the first generates a new one.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::pairing::secret_bytes;

const PUSH_SUBSCRIPTIONS_VERSION: u32 = 1;
const MAX_PUSH_SUBSCRIPTIONS_BYTES: u64 = 256 * 1024;

/// How many devices may hold a push subscription at once.
///
/// Mirrors the reasoning in [`crate::agent_marks`]: a paired device should not
/// be able to grow this file without limit, and forgetting the
/// least-recently-subscribed entry is recoverable in one tap, while refusing
/// the request would strand whoever genuinely reached the limit.
const MAX_PUSH_SUBSCRIPTIONS: usize = 128;

/// How long a push service should hold a wake if the device cannot be reached
/// immediately. Short: a wake that arrives late is a wake about something
/// that may already be resolved, and this is not a payload worth re-trying
/// for long.
const PUSH_TTL_SECONDS: u64 = 60;

/// How long a signed VAPID token is valid for, comfortably under RFC 8292's
/// 24-hour ceiling. Signed fresh for every push rather than cached, since
/// signing is cheap and this is not a hot path.
const VAPID_TOKEN_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// A fixed contact identifying this software to a push service, never the
/// individual self-hoster. Required by RFC 8292 as the VAPID `sub` claim.
const VAPID_CONTACT: &str = "https://github.com/mikro-design/super-herdr";

/// What a browser's `PushManager.subscribe()` returns, flattened: the
/// standard shape nests `p256dh` and `auth` under `keys`, but nothing else
/// here needs that nesting, so the page sends it flat and this is the only
/// shape ever parsed.
///
/// Every field is exactly what the browser produced, used only to address an
/// HTTP request at the subscription's own endpoint. None of it is decoded,
/// inspected, or logged — this daemon is not a party to what it means, only
/// to delivering an empty wake to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSubscription {
    subscription: PushSubscription,
    subscribed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPushSubscriptions {
    version: u32,
    /// Keyed by the SHA-256 of the device's token: the same identity
    /// [`crate::pairing::fingerprint`] and the paired-device list already use,
    /// so a subscription can never be confused with the wrong device and
    /// never needs the device's name, which can change.
    subscriptions: BTreeMap<String, PersistedSubscription>,
}

/// Where paired devices' push subscriptions are kept.
///
/// Deliberately not part of `config.toml`: that file is hand-edited text a
/// person may annotate, and a subscription is a browser-generated blob with
/// no comment worth preserving. Read fresh from disk on every call, the same
/// choice [`crate::daemon::server::DevicePolicy`] makes for devices
/// themselves — a revoked device's subscription stops mattering the moment
/// something next reads this file, not at the next restart.
#[derive(Debug, Clone)]
pub struct PushSubscriptionStore {
    path: PathBuf,
}

impl PushSubscriptionStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME").context(
                "XDG_STATE_HOME or HOME is required to persist Super-Herdr push subscriptions",
            )?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/push-subscriptions.json"),
        })
    }

    /// Use an explicit path instead of the discovered one, the way every
    /// other durable store here lets a test avoid a real user's state.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where [`VapidKeys::load_or_generate`] should persist this
    /// installation's keypair by default: the same directory as the
    /// subscriptions this key signs for, discovered the same way.
    pub fn discover_vapid_key_path() -> Result<PathBuf> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME")
                .context("XDG_STATE_HOME or HOME is required to persist the VAPID key")?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(root.join("super-herdr/push-vapid.json"))
    }

    /// Every currently stored subscription, keyed by device token digest. A
    /// file that cannot be read or parsed is treated as empty rather than
    /// failing the sweep that calls this: a subscription is only ever a
    /// convenience path to a sink that already tried over the live
    /// connection, and losing it costs a device one missed wake, recoverable
    /// by asking again.
    pub fn current(&self) -> BTreeMap<String, PushSubscription> {
        self.load()
            .map(|persisted| persisted.subscriptions)
            .unwrap_or_default()
            .into_iter()
            .map(|(token_sha256, persisted)| (token_sha256, persisted.subscription))
            .collect()
    }

    /// Record or replace one device's subscription.
    pub fn set(&self, token_sha256: &str, subscription: PushSubscription) -> Result<()> {
        let mut persisted = self.load().unwrap_or_else(|_| PersistedPushSubscriptions {
            version: PUSH_SUBSCRIPTIONS_VERSION,
            subscriptions: BTreeMap::new(),
        });
        persisted.subscriptions.insert(
            token_sha256.to_owned(),
            PersistedSubscription {
                subscription,
                subscribed_at_ms: unix_time_ms(),
            },
        );
        enforce_cap(&mut persisted.subscriptions);
        self.save(&persisted)
    }

    /// Forget one device's subscription. Never an error to remove one that
    /// was not there: a browser that unsubscribes twice, or a sweep that
    /// already pruned a dead endpoint, asks for the same end state either
    /// way.
    pub fn remove(&self, token_sha256: &str) -> Result<()> {
        let Ok(mut persisted) = self.load() else {
            return Ok(());
        };
        if persisted.subscriptions.remove(token_sha256).is_none() {
            return Ok(());
        }
        self.save(&persisted)
    }

    fn load(&self) -> Result<PersistedPushSubscriptions> {
        let metadata = fs::metadata(&self.path).context("no push subscriptions are stored")?;
        if metadata.len() > MAX_PUSH_SUBSCRIPTIONS_BYTES {
            bail!("persisted push subscriptions exceed the size limit");
        }
        let bytes = fs::read(&self.path).context("failed to read push subscriptions")?;
        let persisted: PersistedPushSubscriptions =
            serde_json::from_slice(&bytes).context("persisted push subscriptions are invalid")?;
        if persisted.version != PUSH_SUBSCRIPTIONS_VERSION {
            bail!("persisted push subscriptions have an unsupported version");
        }
        if persisted.subscriptions.len() > MAX_PUSH_SUBSCRIPTIONS {
            bail!("persisted push subscriptions name too many devices");
        }
        Ok(persisted)
    }

    fn save(&self, persisted: &PersistedPushSubscriptions) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("push subscriptions path has no parent directory")?;
        fs::create_dir_all(directory)
            .context("failed to create the push subscriptions directory")?;
        crate::attention::set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".push-subscriptions-")
            .tempfile_in(directory)
            .context("failed to create a temporary push subscriptions file")?;
        crate::attention::set_file_permissions(temporary.path())?;
        serde_json::to_writer(&mut temporary, persisted)
            .context("failed to encode push subscriptions")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish push subscriptions")?;
        if temporary
            .as_file()
            .metadata()
            .context("failed to inspect encoded push subscriptions")?
            .len()
            > MAX_PUSH_SUBSCRIPTIONS_BYTES
        {
            bail!("encoded push subscriptions exceed the size limit");
        }
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize push subscriptions")?;
        temporary
            .persist(&self.path)
            .context("failed to atomically replace push subscriptions")?;
        Ok(())
    }
}

fn enforce_cap(subscriptions: &mut BTreeMap<String, PersistedSubscription>) {
    while subscriptions.len() > MAX_PUSH_SUBSCRIPTIONS {
        let Some(oldest) = subscriptions
            .iter()
            .min_by_key(|(token_sha256, persisted)| {
                (persisted.subscribed_at_ms, (*token_sha256).clone())
            })
            .map(|(token_sha256, _)| token_sha256.clone())
        else {
            break;
        };
        subscriptions.remove(&oldest);
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
struct PersistedVapidKey {
    /// The private scalar, raw and unencoded-further: 32 bytes as hex. There
    /// is no format here to be compatible with — this key is read by nothing
    /// but this same daemon.
    private_key_hex: String,
}

/// This installation's VAPID keypair, generated once and signing every push
/// afterward. Never compared, cloned into a subscription, or shown to the
/// device a push is sent to — it identifies the daemon to the push service,
/// not the device to the daemon.
pub struct VapidKeys {
    signing_key: SigningKey,
    public_key_base64url: String,
}

impl VapidKeys {
    /// Load the persisted keypair, or generate and persist one if none exists
    /// yet. A file that exists but cannot be read or parsed is a hard error
    /// rather than a reason to generate a replacement: a silently regenerated
    /// key would leave every already-subscribed device pushing against a
    /// public key the push service no longer honors, with nothing to say why.
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::generate_and_persist(path);
            }
            Err(error) => {
                return Err(error).context("failed to inspect the stored VAPID key");
            }
        }
        let bytes = fs::read(path).context("failed to read the stored VAPID key")?;
        let persisted: PersistedVapidKey =
            serde_json::from_slice(&bytes).context("the stored VAPID key is invalid")?;
        let scalar = hex_decode(&persisted.private_key_hex)
            .context("the stored VAPID key is not valid hex")?;
        let signing_key =
            SigningKey::from_slice(&scalar).context("the stored VAPID key is not a valid key")?;
        Ok(Self::from_signing_key(signing_key))
    }

    fn generate_and_persist(path: &Path) -> Result<Self> {
        // Rejected only if the random 32 bytes are not a valid scalar for the
        // curve, which is astronomically unlikely; retried rather than
        // propagated because a caller cannot do anything about it that trying
        // again does not already do.
        let signing_key = loop {
            let candidate = secret_bytes(32)?;
            if let Ok(key) = SigningKey::from_slice(&candidate) {
                break key;
            }
        };
        let directory = path
            .parent()
            .context("VAPID key path has no parent directory")?;
        fs::create_dir_all(directory).context("failed to create the VAPID key directory")?;
        crate::attention::set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".push-vapid-")
            .tempfile_in(directory)
            .context("failed to create a temporary VAPID key file")?;
        crate::attention::set_file_permissions(temporary.path())?;
        let persisted = PersistedVapidKey {
            private_key_hex: hex_encode(&signing_key.to_bytes()),
        };
        serde_json::to_writer(&mut temporary, &persisted)
            .context("failed to encode the VAPID key")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish the VAPID key")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize the VAPID key")?;
        temporary
            .persist(path)
            .context("failed to atomically write the VAPID key")?;
        Ok(Self::from_signing_key(signing_key))
    }

    fn from_signing_key(signing_key: SigningKey) -> Self {
        let verifying_key = VerifyingKey::from(&signing_key);
        let encoded_point = verifying_key.to_encoded_point(false);
        let public_key_base64url = URL_SAFE_NO_PAD.encode(encoded_point.as_bytes());
        Self {
            signing_key,
            public_key_base64url,
        }
    }

    /// The public key a browser passes as `applicationServerKey`, and a push
    /// service as the VAPID `k` parameter: the uncompressed SEC1 point,
    /// base64url with no padding.
    pub fn public_key_base64url(&self) -> &str {
        &self.public_key_base64url
    }

    /// The `Authorization` header value for a push to this endpoint.
    ///
    /// `aud` must be exactly the endpoint's own scheme and host: a push
    /// service checks the token against the endpoint it received the request
    /// on, and a token signed for the wrong audience is refused.
    fn authorization_header(&self, endpoint: &str) -> Result<String> {
        let audience = endpoint_origin(endpoint)?;
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("the system clock is before 1970")?
            + VAPID_TOKEN_LIFETIME;
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": audience,
            "exp": expires_at.as_secs(),
            "sub": VAPID_CONTACT,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let signature: Signature = self.signing_key.sign(signing_input.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        Ok(format!(
            "vapid t={signing_input}.{signature}, k={}",
            self.public_key_base64url
        ))
    }
}

/// The scheme and host (and port, if not default) a push endpoint is at,
/// exactly what VAPID's `aud` claim names. Mirrors the same extraction
/// [`crate::pairing::pairing_url`] already does for the same reason: neither
/// trusts a URL crate to be here only for this.
fn endpoint_origin(endpoint: &str) -> Result<String> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .context("a push endpoint has no scheme")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        bail!("a push endpoint has no host");
    }
    Ok(format!("{scheme}://{host}"))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("hex-encoded key has an odd length");
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .context("hex-encoded key has an invalid digit")
        })
        .collect()
}

/// Whether sending to this subscription failed in a way that means the push
/// service will never accept it again, so it should be forgotten rather than
/// retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    pub gone: bool,
}

/// Send one empty wake to one subscription. The TTL is short on purpose: this
/// is not a payload worth a push service holding onto and retrying for long,
/// and a wake that finally lands long after it was due is a wake about
/// something that may already be resolved.
pub async fn wake(
    client: &reqwest::Client,
    vapid: &VapidKeys,
    subscription: &PushSubscription,
) -> Result<PushOutcome> {
    let authorization = vapid.authorization_header(&subscription.endpoint)?;
    let response = client
        .post(&subscription.endpoint)
        .header("TTL", PUSH_TTL_SECONDS.to_string())
        .header("Authorization", authorization)
        .header("Content-Length", "0")
        .send()
        .await
        .context("failed to reach the push service")?;
    let status = response.status();
    Ok(PushOutcome {
        gone: status.as_u16() == 404 || status.as_u16() == 410,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(endpoint: &str) -> PushSubscription {
        PushSubscription {
            endpoint: endpoint.to_owned(),
            p256dh: "p256dh-key".to_owned(),
            auth: "auth-secret".to_owned(),
        }
    }

    #[test]
    fn a_store_round_trips_set_and_remove() {
        let directory = tempfile::tempdir().unwrap();
        let store = PushSubscriptionStore::at(directory.path().join("push-subscriptions.json"));

        assert!(store.current().is_empty());

        store
            .set("device-a", subscription("https://fcm.example/send/a"))
            .unwrap();
        store
            .set("device-b", subscription("https://fcm.example/send/b"))
            .unwrap();
        let current = store.current();
        assert_eq!(current.len(), 2);
        assert_eq!(
            current.get("device-a"),
            Some(&subscription("https://fcm.example/send/a"))
        );

        store
            .set("device-a", subscription("https://fcm.example/send/a2"))
            .unwrap();
        assert_eq!(
            store.current().get("device-a"),
            Some(&subscription("https://fcm.example/send/a2"))
        );

        store.remove("device-a").unwrap();
        let remaining = store.current();
        assert_eq!(remaining.len(), 1);
        assert!(!remaining.contains_key("device-a"));

        // Removing twice, or a device that was never subscribed, is not an
        // error: both ask for the same end state.
        store.remove("device-a").unwrap();
        store.remove("never-subscribed").unwrap();
    }

    #[test]
    fn a_full_store_forgets_the_oldest_subscription_first() {
        let directory = tempfile::tempdir().unwrap();
        let store = PushSubscriptionStore::at(directory.path().join("push-subscriptions.json"));

        for index in 0..MAX_PUSH_SUBSCRIPTIONS {
            store
                .set(
                    &format!("device-{index}"),
                    subscription(&format!("https://fcm.example/send/{index}")),
                )
                .unwrap();
        }
        assert_eq!(store.current().len(), MAX_PUSH_SUBSCRIPTIONS);

        store
            .set("device-new", subscription("https://fcm.example/send/new"))
            .unwrap();

        let current = store.current();
        assert_eq!(current.len(), MAX_PUSH_SUBSCRIPTIONS);
        assert!(!current.contains_key("device-0"));
        assert!(current.contains_key("device-new"));
    }

    #[test]
    fn an_unreadable_file_is_treated_as_no_subscriptions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("push-subscriptions.json");
        fs::write(&path, b"not json").unwrap();
        let store = PushSubscriptionStore::at(path);

        assert!(store.current().is_empty());
    }

    #[test]
    fn a_vapid_key_is_generated_once_and_then_stable_across_loads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("push-vapid.json");

        let first = VapidKeys::load_or_generate(&path).unwrap();
        let second = VapidKeys::load_or_generate(&path).unwrap();

        assert_eq!(first.public_key_base64url(), second.public_key_base64url());
    }

    #[test]
    fn a_present_but_corrupt_vapid_key_refuses_rather_than_regenerating() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("push-vapid.json");
        fs::write(&path, b"not a key").unwrap();

        assert!(VapidKeys::load_or_generate(&path).is_err());
    }

    #[test]
    fn a_vapid_public_key_is_sixty_five_bytes_uncompressed() {
        let directory = tempfile::tempdir().unwrap();
        let keys = VapidKeys::load_or_generate(&directory.path().join("push-vapid.json")).unwrap();

        let decoded = URL_SAFE_NO_PAD.decode(keys.public_key_base64url()).unwrap();
        assert_eq!(decoded.len(), 65);
        assert_eq!(decoded[0], 0x04);
    }

    #[test]
    fn an_authorization_header_names_the_endpoints_own_origin() {
        let directory = tempfile::tempdir().unwrap();
        let keys = VapidKeys::load_or_generate(&directory.path().join("push-vapid.json")).unwrap();

        let header = keys
            .authorization_header("https://fcm.googleapis.com/fcm/send/abc123")
            .unwrap();

        assert!(header.starts_with("vapid t="));
        assert!(header.contains(&format!(", k={}", keys.public_key_base64url())));

        let jwt = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split(", k=")
            .next()
            .unwrap();
        let claims_b64 = jwt.split('.').nth(1).unwrap();
        let claims = URL_SAFE_NO_PAD.decode(claims_b64).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&claims).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], VAPID_CONTACT);
    }

    #[test]
    fn endpoint_origin_strips_the_path_and_keeps_the_port() {
        assert_eq!(
            endpoint_origin("https://updates.push.services.mozilla.com:443/wpush/v2/abc").unwrap(),
            "https://updates.push.services.mozilla.com:443"
        );
        assert!(endpoint_origin("not-a-url").is_err());
    }
}
