//! `nixdeploy publish`: renders the manifest, signs it, and writes it next to its detached
//! signature. That is the whole job, and the two things it deliberately does NOT do are the
//! ones a reader will expect it to.
//!
//! **It does not build.** Nothing here evaluates a Nix expression or realises a derivation.
//! Building is the caller's job because building is the only step that requires evaluation,
//! and evaluation is precisely what this project exists to keep off the machines it delivers
//! to (`README.md`, "Not a builder"). Whatever built the closures already knows their store
//! paths; it hands them here as a JSON file.
//!
//! **It does not upload.** The closures must be in a binary cache the receivers trust before
//! a manifest naming them is worth anything, but pushing them there is a cache client's job
//! (`nix copy`, a signing proxy, whatever the operator already runs), and the manifest itself
//! only has to reach a static HTTP origin. Coupling publication to one transport is how a
//! delivery system acquires a single point of failure it did not need -- the same reasoning
//! `modules/default.nix`'s `manifestOutput` states for not serving the manifest either.
//!
//! # Why this lives in the receiver's binary
//!
//! `manifest::supported_schema_version()` and `manifest::known_backends()` are both read
//! from the one file (`lib/schema.json`) `lib/manifest.nix` also reads, so there is no
//! version or backend list for a separate publisher binary to acquire its own copy of and
//! drift from -- and a separate publisher would additionally have its own idea of the
//! manifest's field names, field order and null handling, so a publisher and a receiver
//! could disagree about the bytes while both looking correct in isolation. Here the type
//! that writes a manifest IS the type that reads one (`manifest::ManifestDoc`), the bytes are
//! produced by the one function the receiver verifies against (`manifest::canonical_bytes`),
//! and the key format has both halves in one module. The round-trip test at the bottom of
//! this file is what that buys: publish, then verify, in the same process, over the bytes
//! that actually landed on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::atomicfile::write_atomic;
use crate::manifest::{
    canonical_bytes, known_backends, parse_signing_key, sign_detached, supported_schema_version,
    HostEntry, ManifestDoc,
};

/// Nix's restricted base32 alphabet: digits and lowercase letters EXCLUDING `e`, `o`, `t` and
/// `u`, which upstream leaves out so a hash cannot accidentally spell a word.
const NIX_BASE32: &str = "0123456789abcdfghijklmnpqrsvwxyz";

/// The fields a `hosts.<name>` entry may have. Used to reject a typo'd key in the input file
/// rather than dropping it silently -- `"images"` instead of `"image"` would otherwise
/// publish a machine that can never be reimaged, and nothing would say so until that machine
/// drifted over its ceiling and found the only route out missing.
const HOST_FIELDS: [&str; 3] = ["backend", "path", "image"];

/// Everything `publish` needs, after argument parsing.
#[derive(Debug, Clone)]
pub struct PublishArgs {
    /// JSON file mapping hostname -> `{ backend, path, image? }`. A file rather than repeated
    /// flags because this is the manifest's own `hosts` shape (`lib/manifest.nix`), so the
    /// thing that built the closures emits it directly instead of a shell loop assembling a
    /// second, weaker schema that drifts from the first.
    pub hosts_file: PathBuf,
    pub revision: String,
    /// ISO-8601 UTC, second precision. Defaults to now.
    pub built_at: Option<String>,
    /// File holding the ed25519 secret key. A FILE, never a flag value and never an
    /// environment variable: argv is world-readable through `/proc/<pid>/cmdline` on every
    /// Linux machine, and an environment is readable by anything that can read the process's
    /// own `/proc/<pid>/environ` -- both would put a fleet-signing key in front of every
    /// local user for as long as the publish runs.
    pub signing_key_file: PathBuf,
    /// Where the manifest is written. The detached signature goes to `<out>.sig`, which is
    /// where `manifest::fetch_and_verify` looks for it.
    pub out: PathBuf,
}

/// What a successful publish did, in the same spirit as `outcome::Outcome`: it names the
/// evidence rather than claiming a bare success. In particular it says what was WRITTEN, and
/// nothing about anything being uploaded, served, or fetched -- because none of that
/// happened here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Published {
    pub manifest: PathBuf,
    pub signature: PathBuf,
    pub revision: String,
    pub built_at: String,
    pub hosts: Vec<String>,
    /// Non-fatal things an operator should see. Kept as data rather than printed from deep
    /// inside the call so a test can assert on them without capturing stderr.
    pub warnings: Vec<String>,
}

impl Published {
    pub fn serialize(&self) -> String {
        serde_json::to_string(self).expect("Published always serializes")
    }
}

#[derive(Debug)]
pub enum PublishError {
    Usage(String),
    Read(PathBuf, String),
    Parse(PathBuf, String),
    /// Every problem with the manifest, in one list. A publisher that stops at the first
    /// invalid host makes an operator fix a fleet one error per run.
    Invalid(Vec<String>),
    Key(PathBuf, String),
    Write(PathBuf, String),
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Usage(detail) => write!(f, "{}", detail),
            PublishError::Read(path, e) => write!(f, "reading {}: {}", path.display(), e),
            PublishError::Parse(path, e) => write!(f, "parsing {}: {}", path.display(), e),
            PublishError::Invalid(problems) => {
                writeln!(f, "refusing to publish an invalid manifest:")?;
                for p in problems {
                    writeln!(f, "  - {}", p)?;
                }
                Ok(())
            }
            PublishError::Key(path, e) => write!(f, "signing key {}: {}", path.display(), e),
            PublishError::Write(path, e) => write!(f, "writing {}: {}", path.display(), e),
        }
    }
}

impl std::error::Error for PublishError {}

/// Renders, signs and writes. `now_unix` is passed in rather than read here so `built_at` is
/// pinnable in a test, and so one run cannot straddle a second boundary between rendering and
/// reporting what it rendered.
pub fn publish(args: &PublishArgs, now_unix: u64) -> Result<Published, PublishError> {
    let hosts_text = fs::read_to_string(&args.hosts_file)
        .map_err(|e| PublishError::Read(args.hosts_file.clone(), e.to_string()))?;
    let hosts: BTreeMap<String, HostEntry> = serde_json::from_str(&hosts_text)
        .map_err(|e| PublishError::Parse(args.hosts_file.clone(), e.to_string()))?;

    let mut problems = unknown_host_fields(&hosts_text)
        .map_err(|e| PublishError::Parse(args.hosts_file.clone(), e))?;

    let built_at = args
        .built_at
        .clone()
        .unwrap_or_else(|| iso8601_utc(now_unix));

    let doc = ManifestDoc {
        version: supported_schema_version(),
        revision: args.revision.clone(),
        built_at,
        hosts,
    };
    problems.extend(check(&doc));
    if !problems.is_empty() {
        return Err(PublishError::Invalid(problems));
    }

    let key_text = fs::read_to_string(&args.signing_key_file)
        .map_err(|e| PublishError::Read(args.signing_key_file.clone(), e.to_string()))?;
    let (key_name, signing_key) = parse_signing_key(&key_text)
        .map_err(|e| PublishError::Key(args.signing_key_file.clone(), e))?;

    let mut warnings = Vec::new();
    if let Some(warning) = key_permission_warning(&args.signing_key_file) {
        warnings.push(warning);
    }

    let body = canonical_bytes(&doc);
    let signature = sign_detached(&key_name, &signing_key, body.as_bytes());

    // The signature is written FIRST, and both writes are atomic. A publish is two files, so
    // there is a window in which a reader can see one of them updated and not the other; what
    // this ordering buys is that the window contains the OLD manifest with the NEW signature
    // (a receiver refuses, loudly and safely, and retries on its next tick), and that the
    // moment the manifest lands the pair is already consistent. The reverse order publishes a
    // new target that nobody can verify yet, which is the same refusal arrived at more
    // confusingly.
    let sig_path = signature_path(&args.out);
    write_atomic(&sig_path, format!("{}\n", signature).as_bytes(), 0o644)
        .map_err(|e| PublishError::Write(sig_path.clone(), e))?;

    // Exactly the signed bytes, with nothing appended -- not even the trailing newline a text
    // file usually gets. The receiver verifies what it fetched, untouched (see `manifest.rs`'s
    // "Wire shape"), so one convenience newline added after signing is a fleet-wide refusal.
    write_atomic(&args.out, body.as_bytes(), 0o644)
        .map_err(|e| PublishError::Write(args.out.clone(), e))?;

    Ok(Published {
        manifest: args.out.clone(),
        signature: sig_path,
        revision: doc.revision,
        built_at: doc.built_at,
        hosts: doc.hosts.keys().cloned().collect(),
        warnings,
    })
}

/// Where the detached signature for a manifest at `out` goes: `<out>.sig`, the sibling
/// `manifest::fetch_and_verify` derives from the manifest URL the same way.
pub fn signature_path(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".sig");
    PathBuf::from(name)
}

/// Every problem with a rendered manifest, in one pass -- the Rust half of
/// `lib/manifest.nix`'s `check`, applied to the manifests THIS binary produces. The two are
/// separate implementations of one schema on purpose: a Nix caller assembling a manifest by
/// hand never runs this code, and this binary never evaluates that file.
fn check(doc: &ManifestDoc) -> Vec<String> {
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
        // The publisher-side form of this repo's central rule: a run that delivers to no one
        // must not be able to report success. A manifest with no hosts publishes cleanly,
        // signs cleanly, serves cleanly, and converges nobody -- and every receiver reading
        // it reports the same "no entry for this host" it would report if the publisher had
        // never run at all.
        problems.push(
            "hosts is empty -- a manifest that names no machine converges nobody, and every \
             receiver reading it reports exactly what it would report if this publish had \
             never happened"
                .to_string(),
        );
    }

    for (name, entry) in &doc.hosts {
        let p = |msg: String| format!("host {:?}: {}", name, msg);
        if !known_backends().iter().any(|b| b == &entry.backend) {
            problems.push(p(format!(
                "backend {:?} is not one of: {}",
                entry.backend,
                known_backends().join(", ")
            )));
        }
        if !looks_like_store_path(&entry.path) {
            problems.push(p(format!(
                "path {:?} does not look like a Nix store path",
                entry.path
            )));
        }
        if entry.image.as_deref() == Some("") {
            problems.push(p(
                "image must not be an empty string -- omit it (null) for a host that is never \
                 reimaged"
                    .to_string(),
            ));
        }
    }

    problems
}

/// Names any field in the input file that the manifest schema has no place for. Parsed
/// separately from the typed deserialization because serde silently drops unknown fields, and
/// `#[serde(deny_unknown_fields)]` on the shared `HostEntry` would also make the RECEIVER
/// refuse a manifest carrying a field it does not know -- a strictness that belongs at
/// publish time, where a typo is one person's mistake, not at fetch time, where it is every
/// machine's.
fn unknown_host_fields(hosts_text: &str) -> Result<Vec<String>, String> {
    let raw: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        serde_json::from_str(hosts_text).map_err(|e| e.to_string())?;
    let known: BTreeSet<&str> = HOST_FIELDS.into_iter().collect();

    let mut problems = Vec::new();
    for (name, fields) in raw {
        for field in fields.keys() {
            if !known.contains(field.as_str()) {
                problems.push(format!(
                    "host {:?}: unknown field {:?} (known fields: {})",
                    name,
                    field,
                    HOST_FIELDS.join(", ")
                ));
            }
        }
    }
    Ok(problems)
}

/// `/nix/store/<32 chars of Nix base32>-<name>`. A "looks like a store path" check, not a
/// re-implementation of Nix's own validity rules -- enough to catch the failure that actually
/// happens, which is a manifest naming a URL, a bare package name, an output attribute, or an
/// empty string rather than a subtly malformed path.
fn looks_like_store_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("/nix/store/") else {
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

/// ISO-8601 UTC at second precision, e.g. `2026-08-03T12:00:00Z`. Shape only: a date that
/// parses but does not exist (`2026-02-30`) is not worth a calendar here, because the value
/// this repo actually consumes it for is a human reading a log line.
fn looks_like_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
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

/// Warns when the key file is readable by anyone but its owner. A warning and not a refusal:
/// this binary cannot know whether the file sits on a filesystem with meaningful permission
/// bits at all, and refusing to publish because of a mode it may be misreading would break a
/// deploy over something that is not this program's decision to make.
fn key_permission_warning(path: &Path) -> Option<String> {
    let mode = fs::metadata(path).ok()?.permissions().mode();
    if mode & 0o077 != 0 {
        Some(format!(
            "signing key {} is mode {:o}: readable beyond its owner, which is a fleet-signing \
             key anyone with a local account can copy",
            path.display(),
            mode & 0o7777
        ))
    } else {
        None
    }
}

/// UNIX seconds -> `YYYY-MM-DDTHH:MM:SSZ`, so a publish does not need a calendar dependency
/// for the one timestamp it stamps. The date arithmetic is Howard Hinnant's `civil_from_days`
/// -- proleptic Gregorian, valid far beyond any range this will see, and correct across leap
/// years by construction rather than by a table.
fn iso8601_utc(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let secs = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days land at the END of the 400-year era, which
    // is what removes every special case from the arithmetic below.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Hand-rolled for the same reason `receive.rs`'s is: this binary has two subcommands and a
/// handful of flags, and a CLI crate would be the largest dependency in the tree.
///
/// Unknown flags are an ERROR rather than ignored. A typo in `--built-at` that silently fell
/// back to "now" would publish a manifest whose timestamp is not the one the caller asked
/// for, and nothing downstream would ever notice.
pub fn parse_args(args: &[String]) -> Result<PublishArgs, PublishError> {
    let mut hosts_file: Option<PathBuf> = None;
    let mut revision: Option<String> = None;
    let mut built_at: Option<String> = None;
    let mut signing_key_file: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (args[i].as_str(), None),
        };

        match flag {
            "--hosts" => {
                hosts_file = Some(PathBuf::from(value_of(args, &mut i, "--hosts", inline)?))
            }
            "--revision" => revision = Some(value_of(args, &mut i, "--revision", inline)?),
            "--built-at" => built_at = Some(value_of(args, &mut i, "--built-at", inline)?),
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(value_of(
                    args,
                    &mut i,
                    "--signing-key-file",
                    inline,
                )?))
            }
            "--out" => out = Some(PathBuf::from(value_of(args, &mut i, "--out", inline)?)),
            "--signing-key" | "--key" => {
                return Err(PublishError::Usage(
                    "the signing key is only ever read from a file (--signing-key-file): an \
                     argument is visible in every process listing on the machine for as long \
                     as the publish runs"
                        .to_string(),
                ))
            }
            other => {
                return Err(PublishError::Usage(format!(
                    "unknown flag {:?} -- see `nixdeploy publish` usage",
                    other
                )))
            }
        }
        i += 1;
    }

    Ok(PublishArgs {
        hosts_file: hosts_file
            .ok_or_else(|| PublishError::Usage("--hosts is required".to_string()))?,
        revision: revision
            .ok_or_else(|| PublishError::Usage("--revision is required".to_string()))?,
        built_at,
        signing_key_file: signing_key_file
            .ok_or_else(|| PublishError::Usage("--signing-key-file is required".to_string()))?,
        out: out.ok_or_else(|| PublishError::Usage("--out is required".to_string()))?,
    })
}

/// A flag's value, from either `--flag=value` (already split into `inline`) or the following
/// argument, advancing `i` past it in the second case.
fn value_of(
    args: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<String>,
) -> Result<String, PublishError> {
    match inline {
        Some(v) => Ok(v),
        None => {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| PublishError::Usage(format!("{} needs a value", flag)))
        }
    }
}

pub const USAGE: &str = "\
nixdeploy publish --hosts FILE --revision REV --signing-key-file FILE --out FILE [--built-at TS]

  --hosts FILE             JSON: { \"host-a\": { \"backend\": \"nixos\",
                           \"path\": \"/nix/store/...\", \"image\": null }, ... }
  --revision REV           Opaque build revision recorded in the manifest.
  --signing-key-file FILE  ed25519 secret key, in `nix-store --generate-binary-cache-key`
                           format. A file only: never a flag value, never an environment
                           variable.
  --out FILE               Manifest path. The detached signature is written to FILE.sig.
  --built-at TS            ISO-8601 UTC, e.g. 2026-08-03T12:00:00Z. Defaults to now.

Builds nothing and uploads nothing: the closures must already exist in a cache the
receivers trust, and serving the manifest is somebody else's job.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{verify_and_select, ManifestError};
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use ed25519_dalek::SigningKey;

    const PATH_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system-host-a";
    const PATH_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-system-host-b";

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nixdeploy-publish-{}-{}", tag, std::process::id()));
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    /// The 64-byte libsodium secret key text `nix-store --generate-binary-cache-key` writes,
    /// and the matching public key text a receiver is configured with.
    fn key_texts(seed: [u8; 32]) -> (String, String) {
        let signing = SigningKey::from_bytes(&seed);
        let mut secret = Vec::with_capacity(64);
        secret.extend_from_slice(&seed);
        secret.extend_from_slice(&signing.verifying_key().to_bytes());
        (
            format!("cache-1:{}", BASE64.encode(secret)),
            format!(
                "cache-1:{}",
                BASE64.encode(signing.verifying_key().to_bytes())
            ),
        )
    }

    fn write_key(dir: &Path, seed: [u8; 32]) -> (PathBuf, String) {
        let (secret, public) = key_texts(seed);
        let path = dir.join("signing.key");
        fs::write(&path, &secret).expect("write key");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod key");
        (path, public)
    }

    fn args_for(dir: &Path, hosts_json: &str, key: &Path) -> PublishArgs {
        let hosts_file = dir.join("hosts.json");
        fs::write(&hosts_file, hosts_json).expect("write hosts");
        PublishArgs {
            hosts_file,
            revision: "rev-1".to_string(),
            built_at: Some("2026-08-03T12:00:00Z".to_string()),
            signing_key_file: key.to_path_buf(),
            out: dir.join("manifest.json"),
        }
    }

    fn two_hosts() -> String {
        format!(
            r#"{{"host-b":{{"backend":"nixos","path":"{}","image":"image-b"}},
                 "host-a":{{"backend":"nixos","path":"{}"}}}}"#,
            PATH_B, PATH_A
        )
    }

    #[test]
    fn published_manifest_verifies_as_the_bytes_on_disk() {
        let dir = tmpdir("roundtrip");
        let (key, public) = write_key(&dir, [11u8; 32]);
        let args = args_for(&dir, &two_hosts(), &key);

        let published = publish(&args, 0).expect("publish");
        assert_eq!(published.hosts, vec!["host-a", "host-b"]);
        assert!(
            published.warnings.is_empty(),
            "a 0600 key must not warn: {:?}",
            published.warnings
        );

        // Read the FILES back, exactly as an HTTP fetch would, and run the receiver's own
        // verifier over them. Nothing in this test re-renders or re-serializes anything: if
        // the publisher signed a value rather than the bytes it wrote -- a pretty-printed
        // copy, a trailing newline, a different field order -- this fails.
        let body = fs::read_to_string(&published.manifest).expect("read manifest");
        let sig = fs::read_to_string(&published.signature).expect("read sig");
        let target = verify_and_select(&body, sig.trim(), &public, "host-a").expect("verify");
        assert_eq!(target.store_path, PATH_A);
        assert_eq!(target.image, None, "host-a declared no image");

        let target_b = verify_and_select(&body, sig.trim(), &public, "host-b").expect("verify");
        assert_eq!(target_b.image.as_deref(), Some("image-b"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_signature_covers_every_byte_of_the_written_manifest() {
        // The failure this catches: a publisher that signs one byte string and writes
        // another. Any mutation of the file -- including the single trailing newline a text
        // editor would add without comment -- must make verification fail, because that is
        // exactly what the receiver does with bytes it fetched.
        let dir = tmpdir("bytes");
        let (key, public) = write_key(&dir, [12u8; 32]);
        let args = args_for(&dir, &two_hosts(), &key);
        let published = publish(&args, 0).expect("publish");

        let body = fs::read_to_string(&published.manifest).expect("read manifest");
        let sig = fs::read_to_string(&published.signature).expect("read sig");
        assert!(
            !body.ends_with('\n'),
            "the manifest must be exactly the signed bytes, with nothing appended"
        );

        for mutated in [
            format!("{}\n", body),
            format!(" {}", body),
            body.replacen("rev-1", "rev-2", 1),
        ] {
            let result = verify_and_select(&mutated, sig.trim(), &public, "host-a");
            assert!(
                matches!(result, Err(ManifestError::Signature(_))),
                "mutated manifest verified anyway -- the signature does not cover the bytes \
                 the verifier checks; got {:?}",
                result
            );
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn republishing_identical_input_produces_identical_bytes() {
        let dir = tmpdir("deterministic");
        let (key, _public) = write_key(&dir, [13u8; 32]);

        let first = {
            let args = args_for(&dir, &two_hosts(), &key);
            let p = publish(&args, 0).expect("publish");
            (
                fs::read_to_string(&p.manifest).unwrap(),
                fs::read_to_string(&p.signature).unwrap(),
            )
        };
        // Same hosts, written in the opposite order in the input file: a map whose iteration
        // order leaked into the output would sign to different bytes here, and nothing
        // downstream could ever compare two manifests.
        let reordered = format!(
            r#"{{"host-a":{{"backend":"nixos","path":"{}"}},
                 "host-b":{{"backend":"nixos","path":"{}","image":"image-b"}}}}"#,
            PATH_A, PATH_B
        );
        let second = {
            let args = args_for(&dir, &reordered, &key);
            let p = publish(&args, 0).expect("publish");
            (
                fs::read_to_string(&p.manifest).unwrap(),
                fs::read_to_string(&p.signature).unwrap(),
            )
        };

        assert_eq!(first, second);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_invalid_host_is_reported_in_one_pass_and_nothing_is_written() {
        let dir = tmpdir("invalid");
        let (key, _public) = write_key(&dir, [14u8; 32]);
        let hosts = format!(
            r#"{{"host-a":{{"backend":"kubernetes","path":"{}"}},
                 "host-b":{{"backend":"nixos","path":"not-a-store-path"}},
                 "host-c":{{"backend":"nixos","path":"{}","image":""}},
                 "host-d":{{"backend":"nixos","path":"{}","images":"typo"}}}}"#,
            PATH_A, PATH_B, PATH_A
        );
        let mut args = args_for(&dir, &hosts, &key);
        args.revision = "  ".to_string();

        let err = publish(&args, 0).unwrap_err();
        let PublishError::Invalid(problems) = &err else {
            panic!("want Invalid, got {:?}", err);
        };
        let joined = problems.join("\n");
        for expected in [
            "\"kubernetes\"",
            "not-a-store-path",
            "empty string",
            "unknown field \"images\"",
            "revision",
        ] {
            assert!(
                joined.contains(expected),
                "problem list is missing {:?}:\n{}",
                expected,
                joined
            );
        }

        assert!(
            !args.out.exists() && !signature_path(&args.out).exists(),
            "an invalid publish must leave no manifest and no signature behind"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_manifest_naming_no_machine_is_refused() {
        let dir = tmpdir("empty");
        let (key, _public) = write_key(&dir, [15u8; 32]);
        let args = args_for(&dir, "{}", &key);

        let err = publish(&args, 0).unwrap_err();
        let PublishError::Invalid(problems) = &err else {
            panic!("want Invalid, got {:?}", err);
        };
        assert!(
            problems.iter().any(|p| p.contains("converges nobody")),
            "{:?}",
            problems
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_overbroad_key_file_warns_but_still_publishes() {
        let dir = tmpdir("mode");
        let (key, _public) = write_key(&dir, [16u8; 32]);
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).expect("chmod");

        let published = publish(&args_for(&dir, &two_hosts(), &key), 0).expect("publish");
        assert!(
            published.warnings.iter().any(|w| w.contains("mode 644")),
            "want a permissions warning, got {:?}",
            published.warnings
        );
        assert!(
            published.manifest.exists(),
            "the warning must not block the publish -- this program cannot know whether the \
             mode it read means anything on this filesystem"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bad_signing_key_fails_before_anything_is_written() {
        let dir = tmpdir("badkey");
        let key = dir.join("signing.key");
        fs::write(&key, "cache-1:not-base64!!").expect("write key");
        let args = args_for(&dir, &two_hosts(), &key);

        let err = publish(&args, 0).unwrap_err();
        assert!(matches!(err, PublishError::Key(_, _)), "got {:?}", err);
        assert!(
            !args.out.exists() && !signature_path(&args.out).exists(),
            "a manifest must never be written without the signature that makes it usable"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn built_at_defaults_to_the_supplied_clock() {
        let dir = tmpdir("clock");
        let (key, _public) = write_key(&dir, [17u8; 32]);
        let mut args = args_for(&dir, &two_hosts(), &key);
        args.built_at = None;

        let published = publish(&args, 1_785_758_400).expect("publish");
        assert_eq!(published.built_at, "2026-08-03T12:00:00Z");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iso8601_covers_leap_years_and_the_epoch() {
        // Hand-checked reference points: a hand-rolled calendar that is wrong by a day is
        // wrong in a way nothing else in this repo would ever notice.
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(iso8601_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_735_689_600), "2025-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn store_path_shape_is_checked_against_nix_base32() {
        assert!(looks_like_store_path(PATH_A));
        // 'e' is not in Nix's base32 alphabet, so this is not a hash Nix ever produced.
        assert!(!looks_like_store_path(
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-name"
        ));
        // 31 characters.
        assert!(!looks_like_store_path(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-name"
        ));
        assert!(!looks_like_store_path(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!looks_like_store_path("https://example.org/closure"));
        assert!(!looks_like_store_path("hello"));
        assert!(!looks_like_store_path(""));
    }

    #[test]
    fn timestamp_shape_is_checked() {
        assert!(looks_like_timestamp("2026-08-03T12:00:00Z"));
        assert!(!looks_like_timestamp("2026-08-03 12:00:00Z"));
        assert!(!looks_like_timestamp("2026-08-03T12:00:00+02:00"));
        assert!(!looks_like_timestamp("2026-08-03"));
    }

    #[test]
    fn a_signing_key_may_not_be_passed_as_an_argument() {
        let err = parse_args(&["--signing-key".to_string(), "secret".to_string()]).unwrap_err();
        let PublishError::Usage(detail) = &err else {
            panic!("want Usage, got {:?}", err);
        };
        assert!(
            detail.contains("process listing"),
            "the refusal must say why, got: {}",
            detail
        );
    }

    #[test]
    fn args_parse_in_both_flag_forms_and_reject_typos() {
        let args = parse_args(&[
            "--hosts=/tmp/hosts.json".to_string(),
            "--revision".to_string(),
            "abc".to_string(),
            "--signing-key-file".to_string(),
            "/tmp/key".to_string(),
            "--out=/tmp/manifest.json".to_string(),
        ])
        .expect("parse");
        assert_eq!(args.hosts_file, PathBuf::from("/tmp/hosts.json"));
        assert_eq!(args.revision, "abc");
        assert_eq!(args.out, PathBuf::from("/tmp/manifest.json"));
        assert_eq!(args.built_at, None);

        // A typo'd flag must not be silently ignored: `--build-at` falling back to "now"
        // would publish a timestamp nobody asked for.
        let err = parse_args(&[
            "--hosts".to_string(),
            "/tmp/h".to_string(),
            "--revision".to_string(),
            "r".to_string(),
            "--signing-key-file".to_string(),
            "/tmp/k".to_string(),
            "--out".to_string(),
            "/tmp/m".to_string(),
            "--build-at".to_string(),
            "2026-08-03T12:00:00Z".to_string(),
        ])
        .unwrap_err();
        assert!(matches!(err, PublishError::Usage(_)), "got {:?}", err);

        let missing = parse_args(&["--hosts".to_string(), "/tmp/h".to_string()]).unwrap_err();
        assert!(matches!(missing, PublishError::Usage(_)));
    }

    #[test]
    fn signature_path_is_the_sibling_the_receiver_looks_for() {
        assert_eq!(
            signature_path(Path::new("/srv/www/manifest.json")),
            PathBuf::from("/srv/www/manifest.json.sig")
        );
    }
}
