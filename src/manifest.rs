//! Fetches and verifies the manifest naming every managed machine's target closure, and
//! defines the one Rust shape of that manifest -- the same `ManifestDoc` `publish.rs` fills
//! in and serializes. The schema is defined once, authoritatively, in `lib/manifest.nix`
//! (exposed as `nixdeploy.lib.manifestSchema` -- see `flake.nix`); this module is that
//! schema's one consumer written in a language that is not Nix, so the shapes below are kept
//! in lockstep with it by hand rather than generated from it.
//!
//! There is exactly ONE Rust struct for the manifest, deriving both `Serialize` and
//! `Deserialize`, and both the publisher and the receiver go through it. A separate
//! publisher-side shape would be a third place `SUPPORTED_SCHEMA_VERSION` and the field
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
//! `hosts` attrset, keyed by hostname -- not a document already scoped to one machine, so
//! this receiver determines its own hostname (see `receive.rs`) and looks itself up after the
//! signature and schema version have both already been confirmed.

use std::collections::BTreeMap;
use std::fmt;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The only schema version this receiver understands -- kept equal to `currentVersion` in
/// `lib/manifest.nix` by hand. Bumping one without the other is exactly the drift "refuse
/// unknown schema versions" exists to catch at runtime instead of silently misreading.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

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

/// One `hosts.<name>` entry. `backend` is part of the schema, written by the publisher, and
/// cross-checked against this machine's own `nixdeploy.backend` by nothing in this crate --
/// that would need this module to also know the receiver's configured backend, which is
/// `receive.rs`'s config to carry, not this schema's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    pub backend: String,
    pub path: String,
    /// Always serialized, `null` where absent, matching `lib/manifest.nix`'s `render`
    /// (`image = h.image or null`) -- NOT skipped when `None`. A field that appears only
    /// sometimes is a second manifest shape, and the receiver verifies bytes, so "the same
    /// manifest with the null omitted" is a different document with a different signature.
    #[serde(default)]
    pub image: Option<String>,
}

/// This machine's verified target, extracted from a manifest whose signature and schema
/// version have already been checked, and whose `hosts` map has already been shown to
/// contain this machine's own hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub store_path: String,
    /// The image this machine should be replaced WITH when it turns out it cannot switch to
    /// `store_path` in place -- `hosts.<name>.image` in `lib/manifest.nix`, and the single
    /// argument `provisioningAdapter.reimage` in `modules/default.nix` is specified to take.
    /// This is the argument `receive.rs` hands the configured reimage command after a delta
    /// comes back over the ceiling. `None` means the publisher never expects this machine to
    /// be reimaged; a receiver that HAS a reimage command configured and finds no image here
    /// reports `Stage::Reimage` rather than inventing an image reference of its own.
    pub image: Option<String>,
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
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Fetch(url, e) => write!(f, "fetching {}: {}", url, e),
            ManifestError::Signature(e) => write!(f, "manifest signature: {}", e),
            ManifestError::UnsupportedSchema(v) => write!(
                f,
                "manifest schema version {} is not supported (this receiver understands {})",
                v, SUPPORTED_SCHEMA_VERSION
            ),
            ManifestError::Parse(e) => write!(f, "manifest body: {}", e),
            ManifestError::HostNotFound(host) => {
                write!(
                    f,
                    "manifest has no entry for this host ({:?}) in hosts",
                    host
                )
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

/// The real one: an HTTPS GET. Thin and untested by design -- what needs testing is the
/// verify/parse/select logic every byte it returns then goes through.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        ureq::get(url)
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
) -> Result<Target, ManifestError> {
    let body = fetcher
        .get(url)
        .map_err(|e| ManifestError::Fetch(url.to_string(), e))?;
    let sig_url = format!("{}.sig", url);
    let signature_text = fetcher
        .get(&sig_url)
        .map_err(|e| ManifestError::Fetch(sig_url.clone(), e))?;
    verify_and_select(&body, signature_text.trim(), public_key, hostname)
}

/// The whole verify-then-parse-then-select pipeline, split out from `fetch_and_verify` so it
/// can be unit-tested against in-memory strings without a live HTTP fetch. Every byte of
/// `body` is exactly what was on the wire at `url` -- see the module doc's "Wire shape".
pub(crate) fn verify_and_select(
    body: &str,
    signature_text: &str,
    public_key: &str,
    hostname: &str,
) -> Result<Target, ManifestError> {
    let key = parse_public_key(public_key)?;
    verify(&key, body.as_bytes(), signature_text)?;

    // Only now, after the signature over these exact bytes has verified, is it safe to
    // trust anything the body says.
    let doc: ManifestDoc =
        serde_json::from_str(body).map_err(|e| ManifestError::Parse(e.to_string()))?;
    if doc.version != SUPPORTED_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchema(doc.version));
    }

    let entry = doc
        .hosts
        .get(hostname)
        .ok_or_else(|| ManifestError::HostNotFound(hostname.to_string()))?;

    Ok(Target {
        store_path: entry.path.clone(),
        image: entry.image.clone(),
    })
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
            r#"{{"version":1,"revision":"abc123","builtAt":"2026-08-03T12:00:00Z","hosts":{{"host-a":{{"backend":"nixos","path":"{}","image":null}}}}}}"#,
            host_path
        )
    }

    #[test]
    fn accepts_a_correctly_signed_known_schema_manifest_and_selects_this_host() {
        let (signing, verifying) = keypair();
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &body);

        let target = verify_and_select(&body, &sig, &key_text(&verifying), "host-a")
            .expect("should verify and select");
        assert_eq!(
            target.store_path,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name"
        );
        assert_eq!(target.image, None);
    }

    #[test]
    fn rejects_a_tampered_body() {
        let (signing, verifying) = keypair();
        let real_body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &real_body);
        // Same signature, different bytes underneath it -- the exact tamper this whole
        // module exists to catch before any store path in it is trusted.
        let tampered_body = manifest_body("/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-evil");

        let result = verify_and_select(&tampered_body, &sig, &key_text(&verifying), "host-a");
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

        let result = verify_and_select(body, &sig, &key_text(&verifying), "host-a");
        assert!(matches!(result, Err(ManifestError::UnsupportedSchema(99))));
    }

    #[test]
    fn host_missing_from_manifest_is_reported_distinctly() {
        let (signing, verifying) = keypair();
        let body = manifest_body("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name");
        let sig = sig_text(&signing, &body);

        let result = verify_and_select(&body, &sig, &key_text(&verifying), "some-other-host");
        assert!(matches!(result, Err(ManifestError::HostNotFound(h)) if h == "some-other-host"));
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
        verify_and_select(&body, &sig, &public, "host-a").expect("round trip should verify");
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
        let a = r#"{"version":1,"revision":"r","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-a":{"backend":"nixos","path":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a","image":null},"host-b":{"backend":"nixos","path":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b","image":null}}}"#;
        let b = r#"{"version":1,"revision":"r","builtAt":"2026-08-03T12:00:00Z","hosts":{"host-b":{"backend":"nixos","path":"/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b","image":null},"host-a":{"backend":"nixos","path":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a","image":null}}}"#;

        let doc_a: ManifestDoc = serde_json::from_str(a).expect("parse a");
        let doc_b: ManifestDoc = serde_json::from_str(b).expect("parse b");
        assert_eq!(canonical_bytes(&doc_a), canonical_bytes(&doc_b));
    }
}
