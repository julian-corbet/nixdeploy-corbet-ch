//! Fetches and verifies the manifest naming every managed host plane's target closure, and
//! defines the one Rust shape of that manifest -- the same `ManifestDoc` `publish.rs` fills
//! in and serializes. The schema is defined once, authoritatively, in `lib/manifest.nix`
//! (exposed as `nixdeploy.lib.manifestSchema` -- see `flake.nix`); this module is that
//! schema's one consumer written in a language that is not Nix, so the shapes below are kept
//! in lockstep with the enums below. The shared scalar/list vocabulary lives in
//! `lib/schema.json`; Rust embeds it at compile time and Nix reads the same file at
//! evaluation time.
//!
//! There is exactly ONE Rust struct for the manifest, deriving both `Serialize` and
//! `Deserialize`, and both the publisher and the receiver go through it. A separate
//! publisher-side shape would be a third place the schema version and field
//! names have to be kept in sync by hand -- and unlike the Nix/Rust seam, which at least
//! fails loudly at runtime on a version mismatch, two Rust structs that disagree about a
//! field NAME produce a manifest that verifies, parses, and quietly means something else.
//!
//! The manifest names store paths this receiver will go on to activate, which makes an
//! unverified manifest arbitrary code execution with extra steps -- so the order here is
//! fixed and load-bearing: fetch, verify the signature, THEN parse the body as a manifest,
//! THEN check its schema version, THEN look up this machine's own entry. Nothing before the
//! signature check is allowed to touch a byte that later code trusts.
//!
//! # Wire shape
//!
//! `lib/manifest.nix`'s own header comment is explicit about what gets signed: "`toJSON` on
//! the rendered result is exactly the manifest bytes the publisher signs and the receiver
//! verifies." That is honoured literally here -- the bytes fetched from `manifest.url` are
//! verified AS FETCHED, with nothing unwrapped, re-serialized, or otherwise touched first, so
//! there is no risk of this receiver and the publisher disagreeing about field order or
//! whitespace and silently checking two different byte strings. Consequently the signature
//! cannot live inside that same JSON document (adding a field to it would change the bytes
//! being verified); it travels as a detached sibling file at `<manifest.url>.sig`, using the
//! same `<key-name>:<base64>` text format a narinfo `Sig:` line already uses -- the same
//! reasoning as `parse_public_key`'s doc below.
//!
//! `manifest.url` names ONE combined document for the whole fleet -- `lib/manifest.nix`'s
//! `hosts` attrset, keyed by hostname and then named plane -- not a document already scoped
//! to one receiver, so this receiver determines its own hostname (see `receive.rs`) and uses
//! its configured plane name after the signature and schema version are confirmed.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Schema {
    current_version: u32,
    backends: Vec<String>,
    boot_modes: Vec<String>,
    boot_roles: Vec<String>,
}

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../lib/schema.json"))
        .expect("lib/schema.json must parse in both Rust and Nix")
});

pub fn supported_schema_version() -> u32 {
    SCHEMA.current_version
}

pub fn known_backends() -> &'static [String] {
    &SCHEMA.backends
}

pub fn known_boot_modes() -> &'static [String] {
    &SCHEMA.boot_modes
}

pub fn known_boot_roles() -> &'static [String] {
    &SCHEMA.boot_roles
}

/// Mirrors `lib/manifest.nix`'s `render` output field-for-field. `revision` and `built_at`
/// are part of the schema the receiver must be able to parse without erroring, and neither
/// is consulted for any decision the RECEIVER makes -- they exist for a human or a log line
/// -- but the publisher writes both, which is why they are ordinary fields and not dropped
/// from the struct.
///
/// Field ORDER here is load-bearing in a way a normal `Deserialize` struct's is not: serde
/// serializes in declaration order, so this order is the canonical byte order the publisher
/// signs (see `canonical_bytes`). It matches `lib/manifest.nix`'s `render` so a manifest
/// produced by either side reads the same way to a human diffing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDoc {
    pub version: u32,
    pub revision: String,
    #[serde(rename = "builtAt")]
    pub built_at: String,
    /// `BTreeMap`, never `HashMap`: the publisher serializes this map, and `HashMap`'s
    /// iteration order varies per process, so the same input would sign to different bytes
    /// on every run. Byte-identical output for identical input is what makes a republished
    /// manifest diffable, cacheable, and comparable against what a receiver actually
    /// fetched.
    pub hosts: BTreeMap<String, HostEntry>,
}

/// One `hosts.<name>` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    /// A host may carry several independently-built and independently-activated planes.
    /// The key is the canonical plane name and must equal the backend. Version 3 has exactly
    /// one of each plane kind per host.
    pub planes: BTreeMap<String, PlaneEntry>,
}

/// The activation mechanism for one plane. This is an enum rather than a string so an
/// unknown spelling cannot cross the signed-manifest boundary and reach an adapter chosen
/// for something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Nixos,
    SystemManager,
    HomeManager,
    NixDarwin,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Nixos => "nixos",
            Backend::SystemManager => "system-manager",
            Backend::HomeManager => "home-manager",
            Backend::NixDarwin => "nix-darwin",
        }
    }
}

/// One exact immutable target. `identity` is required only for home-manager because the
/// same machine can activate several users independently. Boot artifacts are orthogonal to
/// that activation target and currently attach only to the NixOS plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlaneEntry {
    pub backend: Backend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    pub target: String,
    /// Boot authority is an independent axis below the NixOS configuration plane. A
    /// managed object may name both roles at once; `none` is the explicit no-actuator
    /// stance used by containers and other externally booted systems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<BootSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BootSpec {
    None,
    Managed { roles: BootRoles },
}

impl BootSpec {
    pub fn mode(&self) -> &'static str {
        match self {
            BootSpec::None => "none",
            BootSpec::Managed { .. } => "managed",
        }
    }

    pub fn artifact(&self, role: BootRole) -> Option<&BootArtifact> {
        match (self, role) {
            (BootSpec::Managed { roles }, BootRole::Primary) => Some(&roles.primary),
            (BootSpec::Managed { roles }, BootRole::Nixrescue) => roles.nixrescue.as_ref(),
            (BootSpec::None, _) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootRoles {
    pub primary: BootArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixrescue: Option<BootArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootArtifact {
    /// Exact nixboot-produced store artifact. This is independent of the configuration
    /// plane's activation target and is always signed as part of the manifest.
    pub artifact: String,
    /// Provider-native immutable image reference derived from this artifact, when one
    /// exists. Private Infra supplies the value; nixdeploy only transports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootRole {
    Primary,
    Nixrescue,
}

impl BootRole {
    pub fn as_str(self) -> &'static str {
        match self {
            BootRole::Primary => "primary",
            BootRole::Nixrescue => "nixrescue",
        }
    }
}

impl fmt::Display for BootRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// This receiver's verified plane target, extracted from a manifest whose signature and
/// schema version have already been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub plane: String,
    pub backend: Backend,
    pub identity: Option<String>,
    pub store_path: String,
    pub boot: Option<BootSpec>,
}

#[derive(Debug)]
pub enum ManifestError {
    Fetch(String, String),
    Signature(String),
    UnsupportedSchema(u32),
    Parse(String),
    /// This machine's own hostname is not a key in the manifest's `hosts` map -- the
    /// publisher does not know this machine exists (yet, or at all).
    HostNotFound(String),
    PlaneNotFound {
        host: String,
        plane: String,
    },
    Invalid(Vec<String>),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Fetch(url, e) => write!(f, "fetching {}: {}", url, e),
            ManifestError::Signature(e) => write!(f, "manifest signature: {}", e),
            ManifestError::UnsupportedSchema(v) => write!(
                f,
                "manifest schema version {} is not supported (this receiver understands {})",
                v,
                supported_schema_version()
            ),
            ManifestError::Parse(e) => write!(f, "manifest body: {}", e),
            ManifestError::HostNotFound(host) => {
                write!(
                    f,
                    "manifest has no entry for this host ({:?}) in hosts",
                    host
                )
            }
            ManifestError::PlaneNotFound { host, plane } => {
                write!(f, "manifest host {:?} has no plane {:?}", host, plane)
            }
            ManifestError::Invalid(problems) => {
                writeln!(
                    f,
                    "manifest does not satisfy schema version {}:",
                    supported_schema_version()
                )?;
                for problem in problems {
                    writeln!(f, "  - {}", problem)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// Where the manifest bytes come from. A trait rather than a hardcoded HTTP call so the
/// whole receiver pipeline -- manifest, delta, activation -- can be driven end to end in a
/// test against bytes a test produced, without a listening socket. Before this existed, the
/// only tested part of that pipeline was each stage in isolation, which is precisely how a
/// pipeline that works stage-by-stage still fails as a whole.
pub trait Fetcher {
    /// Returns the body at `url` as text, or a human-readable reason it could not.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Every network read that gates a receiver run is bounded. Fetching happens before any
/// activation state is touched, so a timeout is a safe failed run that the next timer tick
/// can retry; an unbounded read can silently stop convergence forever.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The real one: an HTTPS GET. Thin and untested by design -- what needs testing is the
/// verify/parse/select logic every byte it returns then goes through.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        ureq::get(url)
            .timeout(FETCH_TIMEOUT)
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())
    }
}

/// Fetches `url` and its detached signature at `<url>.sig` through `fetcher`, verifies the
/// signature against `public_key`, and returns `hostname`'s target -- or an error at
/// whichever step first failed to hold.
pub fn fetch_and_verify(
    fetcher: &dyn Fetcher,
    url: &str,
    public_key: &str,
    hostname: &str,
    plane: &str,
) -> Result<Target, ManifestError> {
    let body = fetcher
        .get(url)
        .map_err(|e| ManifestError::Fetch(url.to_string(), e))?;
    let sig_url = format!("{}.sig", url);
    let signature_text = fetcher
        .get(&sig_url)
        .map_err(|e| ManifestError::Fetch(sig_url.clone(), e))?;
    verify_and_select(&body, signature_text.trim(), public_key, hostname, plane)
}

/// The whole verify-then-parse-then-select pipeline, split out from `fetch_and_verify` so it
/// can be unit-tested against in-memory strings without a live HTTP fetch. Every byte of
/// `body` is exactly what was on the wire at `url` -- see the module doc's "Wire shape".
pub(crate) fn verify_and_select(
    body: &str,
    signature_text: &str,
    public_key: &str,
    hostname: &str,
    plane: &str,
) -> Result<Target, ManifestError> {
    let key = parse_public_key(public_key)?;
    verify(&key, body.as_bytes(), signature_text)?;

    // Only now, after the signature over these exact bytes has verified, is it safe to
    // trust anything the body says.
    let doc: ManifestDoc =
        serde_json::from_str(body).map_err(|e| ManifestError::Parse(e.to_string()))?;
    if doc.version != supported_schema_version() {
        return Err(ManifestError::UnsupportedSchema(doc.version));
    }

    let problems = validate(&doc);
    if !problems.is_empty() {
        return Err(ManifestError::Invalid(problems));
    }

    let host = doc
        .hosts
        .get(hostname)
        .ok_or_else(|| ManifestError::HostNotFound(hostname.to_string()))?;
    let entry = host
        .planes
        .get(plane)
        .ok_or_else(|| ManifestError::PlaneNotFound {
            host: hostname.to_string(),
            plane: plane.to_string(),
        })?;

    Ok(Target {
        plane: plane.to_string(),
        backend: entry.backend,
        identity: entry.identity.clone(),
        store_path: entry.target.clone(),
        boot: entry.boot.clone(),
    })
}

/// Validates the semantic rules that serde cannot express. The publisher and receiver both
/// call this exact function: the publisher catches mistakes before signing, while the
/// receiver remains safe when a third-party publisher implements the Nix schema directly.
pub fn validate(doc: &ManifestDoc) -> Vec<String> {
    let mut problems = Vec::new();
    if doc.revision.trim().is_empty() {
        problems.push("revision must be a non-empty string".to_string());
    }
    if !looks_like_timestamp(&doc.built_at) {
        problems.push(format!(
            "builtAt {:?} is not an ISO-8601 UTC timestamp, e.g. 2026-08-03T12:00:00Z",
            doc.built_at
        ));
    }
    if doc.hosts.is_empty() {
        problems.push("hosts must contain at least one host".to_string());
    }

    for (host_name, host) in &doc.hosts {
        if host_name.trim().is_empty() {
            problems.push("host name must not be empty".to_string());
        }
        if host.planes.is_empty() {
            problems.push(format!("host {:?}: planes must not be empty", host_name));
        }
        for (plane_name, plane) in &host.planes {
            let prefix =
                |message: &str| format!("host {:?} plane {:?}: {}", host_name, plane_name, message);
            if plane_name != plane.backend.as_str() {
                problems.push(prefix(&format!(
                    "name must equal backend {:?}; the canonical plane names are nixos, system-manager, home-manager and nix-darwin",
                    plane.backend.as_str()
                )));
            }
            if !looks_like_store_path(&plane.target) {
                problems.push(prefix("target does not look like a Nix store path"));
            }
            match plane.backend {
                Backend::HomeManager => match plane.identity.as_deref() {
                    Some(identity) if !identity.trim().is_empty() => {}
                    _ => problems.push(prefix(
                        "identity is required and must be non-empty for home-manager",
                    )),
                },
                _ if plane.identity.is_some() => problems.push(prefix(
                    "identity is only meaningful for the home-manager backend",
                )),
                _ => {}
            }
            match (&plane.boot, plane.backend) {
                (None, Backend::Nixos) => problems.push(prefix(
                    "boot is required for nixos; use mode none when no boot actuator exists",
                )),
                (Some(_), backend)
                    if !matches!(backend, Backend::Nixos | Backend::SystemManager) =>
                {
                    problems.push(prefix(
                        "boot is valid only for nixos and system-manager system planes",
                    ))
                }
                (Some(BootSpec::Managed { roles }), Backend::Nixos | Backend::SystemManager) => {
                    for (role, artifact) in [
                        (BootRole::Primary, Some(&roles.primary)),
                        (BootRole::Nixrescue, roles.nixrescue.as_ref()),
                    ] {
                        let Some(artifact) = artifact else { continue };
                        if !looks_like_store_path(&artifact.artifact) {
                            problems.push(prefix(&format!(
                                "boot role {} artifact does not look like a Nix store path",
                                role
                            )));
                        }
                        if artifact.image.as_deref() == Some("") {
                            problems.push(prefix(&format!(
                                "boot role {} image must not be empty",
                                role
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    problems
}

const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

pub fn looks_like_store_path(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("/nix/store/") else {
        return false;
    };
    let Some((hash, name)) = rest.split_once('-') else {
        return false;
    };
    hash.len() == 32
        && hash.chars().all(|c| NIX_BASE32.contains(c))
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+_.?=-".contains(c))
}

fn looks_like_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 20 {
        return false;
    }
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let literals = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'Z'),
    ];
    digits.iter().all(|&i| b[i].is_ascii_digit()) && literals.iter().all(|&(i, c)| b[i] == c)
}

/// The manifest's canonical byte form: exactly the bytes the publisher signs and writes,
/// and exactly the bytes a receiver verifies as fetched. Compact JSON (no pretty-printing,
/// no trailing newline) in this struct's declaration order, with `hosts` in `BTreeMap` key
/// order.
///
/// "Canonical" here means only that identical input produces identical bytes -- it is NOT a
/// JSON canonicalization scheme, and nothing re-serializes a parsed manifest to compare it
/// against these bytes. The receiver never re-serializes at all (see the module doc), which
/// is what makes that distinction safe: the only producer of signed bytes is this function,
/// so the only way to get a mismatch is to modify the bytes after signing them. That is the
/// one thing `publish.rs` is careful never to do -- not even appending the trailing newline
/// a text file usually wants, because a manifest with a newline the signature does not cover
/// is a manifest every receiver in the fleet refuses at once.
pub fn canonical_bytes(doc: &ManifestDoc) -> String {
    serde_json::to_string(doc).expect("ManifestDoc always serializes: strings, ints, and maps")
}

/// Parses the `<name>:<base64>` text format `nix-store --generate-binary-cache-key` already
/// produces (the same format a narinfo `Sig:` line uses) -- reusing it here means an operator
/// who already manages a binary cache signing key is not asked to learn a second key format
/// for this one purpose. A bare base64 key with no `name:` prefix is accepted too, since the
/// name is never actually checked against anything (see below).
fn parse_public_key(text: &str) -> Result<VerifyingKey, ManifestError> {
    let encoded = match text.split_once(':') {
        Some((_name, rest)) => rest,
        None => text,
    };
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| ManifestError::Signature(format!("public key is not valid base64: {}", e)))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        ManifestError::Signature(format!(
            "public key is {} bytes, want 32 (a raw ed25519 public key)",
            v.len()
        ))
    })?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| {
        ManifestError::Signature(format!("public key is not a valid ed25519 key: {}", e))
    })
}

/// Same `<name>:<base64>` format as `parse_public_key`, applied to the `.sig` file's
/// contents. The name prefix, if any, is not checked against the key's own name -- there is
/// exactly one key configured per receiver (`nixdeploy.receiver.manifest.publicKey`), so a
/// name mismatch would only ever be cosmetic; what matters is whether the bytes verify.
fn verify(key: &VerifyingKey, message: &[u8], signature_text: &str) -> Result<(), ManifestError> {
    let encoded = match signature_text.split_once(':') {
        Some((_name, rest)) => rest,
        None => signature_text,
    };
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| ManifestError::Signature(format!("signature is not valid base64: {}", e)))?;
    let bytes: [u8; 64] = bytes.try_into().map_err(|v: Vec<u8>| {
        ManifestError::Signature(format!(
            "signature is {} bytes, want 64 (a raw ed25519 signature)",
            v.len()
        ))
    })?;
    let signature = Signature::from_bytes(&bytes);
    key.verify(message, &signature)
        .map_err(|e| ManifestError::Signature(format!("signature does not verify: {}", e)))
}

/// Verifies an existing manifest with the public half of the signing key a partial
/// publisher is about to use. This prevents a locally tampered base manifest from being
/// blessed with a fresh signature while preserving unselected targets.
pub fn verify_with_signing_key(
    key: &SigningKey,
    body: &[u8],
    signature_text: &str,
) -> Result<(), ManifestError> {
    verify(&key.verifying_key(), body, signature_text)
}

/// Parses a SECRET key in the same `<name>:<base64>` text format as `parse_public_key`, i.e.
/// the file `nix-store --generate-binary-cache-key` writes for the private half. Both halves
/// of the key format live in this one module on purpose: the publisher writing a signature
/// the receiver cannot parse is a failure that shows up only on the machines, at fetch time,
/// after a real deploy has already been published.
///
/// Two encodings are accepted, and the difference matters:
///
///   * 64 bytes -- libsodium's `crypto_sign` secret key, which is a 32-byte seed followed by
///     a copy of its own 32-byte public key. This is what Nix's key generator writes. The
///     trailing public half is CHECKED against the one derived from the seed rather than
///     ignored: a file whose two halves disagree is not an ed25519 secret key at all (a
///     truncated concatenation, two keys spliced together, a key from another algorithm),
///     and signing with the seed anyway would produce signatures that verify against a
///     public key nobody has published -- i.e. every receiver refuses, and the operator's
///     first clue is a fleet-wide outage rather than a failed publish.
///   * 32 bytes -- a bare ed25519 seed, for an operator who generated one without Nix.
///
/// Returns the key's own name alongside it, so the detached signature can carry the same
/// `<name>:` prefix the key file uses.
pub fn parse_signing_key(text: &str) -> Result<(String, SigningKey), String> {
    let text = text.trim();
    let (name, encoded) = match text.split_once(':') {
        Some((name, rest)) => (name.to_string(), rest),
        None => (String::new(), text),
    };
    let bytes = BASE64
        .decode(encoded)
        .map_err(|e| format!("signing key is not valid base64: {}", e))?;

    let seed: [u8; 32] = match bytes.len() {
        32 => bytes[..32].try_into().expect("checked length"),
        64 => {
            let seed: [u8; 32] = bytes[..32].try_into().expect("checked length");
            let claimed_public = &bytes[32..];
            let derived = SigningKey::from_bytes(&seed).verifying_key();
            if derived.to_bytes() != claimed_public {
                return Err(
                    "signing key's trailing public half does not match the key derived from \
                     its seed -- this is not a libsodium ed25519 secret key, and signing with \
                     it would produce signatures no published public key can verify"
                        .to_string(),
                );
            }
            seed
        }
        n => {
            return Err(format!(
                "signing key is {} bytes, want 64 (a libsodium ed25519 secret key, as \
                 `nix-store --generate-binary-cache-key` writes) or 32 (a bare seed)",
                n
            ))
        }
    };

    Ok((name, SigningKey::from_bytes(&seed)))
}

/// Signs `message` and renders the detached signature in the same `<name>:<base64>` text
/// format `verify` above parses. `name` is cosmetic (see `verify`'s doc -- the name is never
/// checked), but carrying the key's own name through means a `.sig` file next to a manifest
/// says which key an operator has to look for.
pub fn sign_detached(name: &str, key: &SigningKey, message: &[u8]) -> String {
    let signature = key.sign(message);
    if name.is_empty() {
        BASE64.encode(signature.to_bytes())
    } else {
        format!("{}:{}", name, BASE64.encode(signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_schema_vocabulary_matches_the_rust_enums() {
        assert_eq!(supported_schema_version(), 3);
        assert_eq!(known_boot_modes(), ["none", "managed"]);

        for name in known_backends() {
            let parsed: Backend =
                serde_json::from_str(&format!("{:?}", name)).expect("known backend parses");
            assert_eq!(parsed.as_str(), name);
        }
        for name in known_boot_roles() {
            let parsed: BootRole =
                serde_json::from_str(&format!("{:?}", name)).expect("known boot role parses");
            assert_eq!(parsed.as_str(), name);
        }
    }

    fn keypair() -> (SigningKey, VerifyingKey) {
        // Fixed seed: these tests only need a valid, deterministic ed25519 keypair, never a
        // secure one.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn key_text(key: &VerifyingKey) -> String {
        format!("test-key:{}", BASE64.encode(key.to_bytes()))
    }

    fn sig_text(signing: &SigningKey, body: &str) -> String {
        let sig = signing.sign(body.as_bytes());
        format!("test-key:{}", BASE64.encode(sig.to_bytes()))
    }

    fn manifest_body(host_path: &str) -> String {
        format!(
            r#"{{"version":{},"revision":"abc123","builtAt":"2026-08-03T12:00:00Z","hosts":{{"host-a":{{"planes":{{"nixos":{{"backend":"nixos","target":"{}","boot":{{"mode":"none"}}}}}}}}}}}}"#,
            supported_schema_version(),
            host_path,
        )
    }

    #[test]
    fn accepts_a_correctly_signed_known_schema_manifest_and_selects_this_host() {
        let (signing, verifying) = keypair();
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &body);

        let target = verify_and_select(&body, &sig, &key_text(&verifying), "host-a", "nixos")
            .expect("should verify and select");
        assert_eq!(
            target.store_path,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name"
        );
        assert_eq!(target.boot, Some(BootSpec::None));
        assert_eq!(target.backend, Backend::Nixos);
    }

    #[test]
    fn rejects_a_tampered_body() {
        let (signing, verifying) = keypair();
        let real_body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &real_body);
        // Same signature, different bytes underneath it -- the exact tamper this whole
        // module exists to catch before any store path in it is trusted.
        let tampered_body = manifest_body("/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-evil");

        let result = verify_and_select(
            &tampered_body,
            &sig,
            &key_text(&verifying),
            "host-a",
            "nixos",
        );
        assert!(
            matches!(result, Err(ManifestError::Signature(_))),
            "want a Signature error, got {:?}",
            result
        );
    }

    #[test]
    fn unsupported_schema_version_is_refused() {
        let (signing, verifying) = keypair();
        let body = r#"{"version":99,"revision":"abc","builtAt":"2026-08-03T12:00:00Z","hosts":{}}"#;
        let sig = sig_text(&signing, body);

        let result = verify_and_select(body, &sig, &key_text(&verifying), "host-a", "nixos");
        assert!(matches!(result, Err(ManifestError::UnsupportedSchema(99))));
    }

    #[test]
    fn host_missing_from_manifest_is_reported_distinctly() {
        let (signing, verifying) = keypair();
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &body);

        let result = verify_and_select(
            &body,
            &sig,
            &key_text(&verifying),
            "some-other-host",
            "nixos",
        );
        assert!(matches!(result, Err(ManifestError::HostNotFound(h)) if h == "some-other-host"));
    }

    #[test]
    fn plane_missing_from_a_known_host_is_reported_distinctly() {
        let (signing, verifying) = keypair();
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &body);

        let result =
            verify_and_select(&body, &sig, &key_text(&verifying), "host-a", "home-manager");
        assert!(matches!(
            result,
            Err(ManifestError::PlaneNotFound { host, plane })
                if host == "host-a" && plane == "home-manager"
        ));
    }

    #[test]
    fn home_manager_identity_is_part_of_the_signed_target() {
        let (signing, verifying) = keypair();
        let body = r#"{"version":3,"revision":"abc123","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-a":{"planes":{"home-manager":{"backend":"home-manager","identity":"alice","target":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-home-manager-generation"}}}}}"#;
        let sig = sig_text(&signing, body);

        let target = verify_and_select(body, &sig, &key_text(&verifying), "host-a", "home-manager")
            .expect("select home-manager plane");
        assert_eq!(target.backend, Backend::HomeManager);
        assert_eq!(target.identity.as_deref(), Some("alice"));
    }

    #[test]
    fn a_signed_home_manager_plane_without_identity_is_still_invalid() {
        let (signing, verifying) = keypair();
        let body = r#"{"version":3,"revision":"abc123","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-a":{"planes":{"home-manager":{"backend":"home-manager","target":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-home-manager-generation"}}}}}"#;
        let sig = sig_text(&signing, body);

        let result = verify_and_select(body, &sig, &key_text(&verifying), "host-a", "home-manager");
        assert!(matches!(result, Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn plane_name_must_be_the_canonical_backend_name() {
        let (signing, verifying) = keypair();
        let body = r#"{"version":3,"revision":"abc123","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-a":{"planes":{"system":{"backend":"nixos","target":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system","boot":{"mode":"none"}}}}}}"#;
        let sig = sig_text(&signing, body);

        let result = verify_and_select(body, &sig, &key_text(&verifying), "host-a", "system");
        assert!(matches!(result, Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn public_key_without_name_prefix_is_also_accepted() {
        let (_signing, verifying) = keypair();
        let bare = BASE64.encode(verifying.to_bytes());
        parse_public_key(&bare).expect("bare base64 key should parse");
    }

    #[test]
    fn public_key_with_wrong_length_is_rejected() {
        let err = parse_public_key("name:AAAA").unwrap_err();
        assert!(matches!(err, ManifestError::Signature(_)));
    }

    /// The 64-byte libsodium form Nix's own key generator writes: seed followed by the
    /// public key derived from it.
    fn libsodium_secret_text(name: &str, seed: [u8; 32]) -> String {
        let signing = SigningKey::from_bytes(&seed);
        let mut raw = Vec::with_capacity(64);
        raw.extend_from_slice(&seed);
        raw.extend_from_slice(&signing.verifying_key().to_bytes());
        format!("{}:{}", name, BASE64.encode(raw))
    }

    #[test]
    fn signing_key_and_verifying_key_are_the_same_format_and_round_trip() {
        let (name, signing) = parse_signing_key(&libsodium_secret_text("cache-1", [9u8; 32]))
            .expect("libsodium secret key should parse");
        assert_eq!(name, "cache-1", "the key's own name must survive parsing");

        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sign_detached(&name, &signing, body.as_bytes());
        assert!(
            sig.starts_with("cache-1:"),
            "detached signature should carry the key's name, got {}",
            sig
        );

        // The whole point: bytes signed by the publisher half verify through the receiver
        // half, with neither side reformatting anything in between.
        let public = key_text(&signing.verifying_key());
        verify_and_select(&body, &sig, &public, "host-a", "nixos")
            .expect("round trip should verify");
    }

    #[test]
    fn signing_key_with_mismatched_public_half_is_rejected() {
        // A 64-byte key whose trailing half is NOT the public key for its seed -- e.g. two
        // different keys spliced together, or a truncated copy. Signing with it would
        // produce signatures that verify against a key nobody published, so every receiver
        // would refuse and the publish itself would look fine.
        let mut raw = Vec::with_capacity(64);
        raw.extend_from_slice(&[9u8; 32]);
        raw.extend_from_slice(
            &SigningKey::from_bytes(&[3u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        let text = format!("cache-1:{}", BASE64.encode(raw));

        let err = parse_signing_key(&text).unwrap_err();
        assert!(
            err.contains("public half"),
            "want a mismatch complaint, got: {}",
            err
        );
    }

    #[test]
    fn bare_32_byte_seed_is_accepted_as_a_signing_key() {
        let text = format!("seed-only:{}", BASE64.encode([5u8; 32]));
        let (_name, signing) = parse_signing_key(&text).expect("bare seed should parse");
        assert_eq!(
            signing.to_bytes(),
            [5u8; 32],
            "a bare seed must be used as the seed, not reinterpreted"
        );
    }

    #[test]
    fn signing_key_of_the_wrong_length_is_rejected() {
        let err = parse_signing_key("k:AAAA").unwrap_err();
        assert!(err.contains("bytes"), "got: {}", err);
    }

    #[test]
    fn canonical_bytes_are_the_wire_shape_this_module_parses() {
        // Pins the publisher's output to the hand-written JSON these tests verify against,
        // which is itself written to match `lib/manifest.nix`'s `render`. If a field is
        // renamed, reordered, or starts being skipped when null, this fails HERE -- at
        // build time, in one place -- instead of on every receiver at once, as a signature
        // that verifies over bytes nobody expected.
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let doc: ManifestDoc = serde_json::from_str(&body).expect("parse");
        assert_eq!(canonical_bytes(&doc), body);
    }

    #[test]
    fn canonical_bytes_do_not_depend_on_input_key_order() {
        // Two manifests differing only in the ORDER their host keys were written must sign
        // to the same bytes -- otherwise republishing an unchanged fleet produces a
        // different signature every time and nothing downstream can compare two manifests.
        let a = r#"{"version":3,"revision":"r","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-a":{"planes":{"nixos":{"backend":"nixos","target":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a","boot":{"mode":"none"}}}},"host-b":{"planes":{"nixos":{"backend":"nixos","target":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b","boot":{"mode":"none"}}}}}}"#;
        let b = r#"{"version":3,"revision":"r","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-b":{"planes":{"nixos":{"backend":"nixos","target":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b","boot":{"mode":"none"}}}},"host-a":{"planes":{"nixos":{"backend":"nixos","target":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a","boot":{"mode":"none"}}}}}}"#;

        let doc_a: ManifestDoc = serde_json::from_str(a).expect("parse a");
        let doc_b: ManifestDoc = serde_json::from_str(b).expect("parse b");
        assert_eq!(canonical_bytes(&doc_a), canonical_bytes(&doc_b));
    }
}
