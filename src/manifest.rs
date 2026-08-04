//! Fetches and verifies the manifest naming every managed machine's target closure. The
//! schema is defined once, authoritatively, in `lib/manifest.nix` (exposed as
//! `nixdeploy.lib.manifestSchema` -- see `flake.nix`); this module is that schema's one
//! consumer written in a language that is not Nix, so the shapes below are kept in lockstep
//! with it by hand rather than generated from it.
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
//! this receiver determines its own hostname (see `main.rs`) and looks itself up after the
//! signature and schema version have both already been confirmed.

use std::collections::HashMap;
use std::fmt;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

/// The only schema version this receiver understands -- kept equal to `currentVersion` in
/// `lib/manifest.nix` by hand. Bumping one without the other is exactly the drift "refuse
/// unknown schema versions" exists to catch at runtime instead of silently misreading.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// Mirrors `lib/manifest.nix`'s `render` output field-for-field. `revision` and `built_at`
/// are part of the schema this receiver must be able to parse without erroring, but neither
/// is consulted for any decision this crate makes -- they exist for a human or a log line,
/// not for `fetch_and_verify`'s own logic -- so both are `#[allow(dead_code)]` rather than
/// silently dropped from the struct (dropping them would make this receiver's understanding
/// of the schema incomplete for anyone who reads this file to see what a manifest contains).
#[derive(Debug, Deserialize)]
struct ManifestDoc {
    version: u32,
    #[allow(dead_code)]
    revision: String,
    #[allow(dead_code)]
    #[serde(rename = "builtAt")]
    built_at: String,
    hosts: HashMap<String, HostEntry>,
}

/// One `hosts.<name>` entry. `backend` is part of the schema (and cross-checked against this
/// machine's own `nixdeploy.backend` by nothing in this crate -- that would need this
/// module to also know the receiver's configured backend, which is `main.rs`'s config to
/// carry, not this schema's) so it is kept but not read.
#[derive(Debug, Deserialize)]
struct HostEntry {
    #[allow(dead_code)]
    backend: String,
    path: String,
    #[serde(default)]
    image: Option<String>,
}

/// This machine's verified target, extracted from a manifest whose signature and schema
/// version have already been checked, and whose `hosts` map has already been shown to
/// contain this machine's own hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub store_path: String,
    /// The image this machine should be running from, where applicable (see
    /// `provisioningAdapter.imageRef` in `modules/default.nix`). Not consumed by today's
    /// receiver -- it has no reimage adapter of its own (see `outcome::Outcome::Reimaged`'s
    /// doc: reimage runs on the publisher side against a machine that may be unreachable) --
    /// but carried through because the manifest is the one contract anything that DOES act
    /// on a refusal-by-reimaging reads from.
    #[allow(dead_code)]
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

/// Fetches `url` and its detached signature at `<url>.sig` over HTTPS, verifies the
/// signature against `public_key`, and returns `hostname`'s target -- or an error at
/// whichever step first failed to hold.
pub fn fetch_and_verify(
    url: &str,
    public_key: &str,
    hostname: &str,
) -> Result<Target, ManifestError> {
    let body = fetch(url)?;
    let sig_url = format!("{}.sig", url);
    let signature_text = fetch(&sig_url)?;
    verify_and_select(&body, signature_text.trim(), public_key, hostname)
}

/// The whole verify-then-parse-then-select pipeline, split out from `fetch_and_verify` so it
/// can be unit-tested against in-memory strings without a live HTTP fetch. Every byte of
/// `body` is exactly what was on the wire at `url` -- see the module doc's "Wire shape".
fn verify_and_select(
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

fn fetch(url: &str) -> Result<String, ManifestError> {
    ureq::get(url)
        .call()
        .map_err(|e| ManifestError::Fetch(url.to_string(), e.to_string()))?
        .into_string()
        .map_err(|e| ManifestError::Fetch(url.to_string(), e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

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
}
