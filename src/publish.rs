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
//! `manifest::SUPPORTED_SCHEMA_VERSION` is already kept in sync with `lib/manifest.nix` by
//! hand. A separate publisher would make that three places, and would additionally have its
//! own idea of the manifest's field names, field order and null handling -- so a publisher
//! and a receiver could disagree about the bytes while both looking correct in isolation.
//! Here the type that writes a manifest IS the type that reads one (`manifest::ManifestDoc`),
//! the bytes are produced by the one function the receiver verifies against
//! (`manifest::canonical_bytes`), and the key format has both halves in one module. The
//! round-trip test at the bottom of this file is what that buys: publish, then verify, in
//! the same process, over the bytes that actually landed on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::atomicfile::write_atomic;
use crate::manifest::{
    canonical_bytes, parse_signing_key, sign_detached, validate, verify_with_signing_key,
    HostEntry, ManifestDoc, PlaneEntry, SUPPORTED_SCHEMA_VERSION,
};

/// Everything `publish` needs, after argument parsing.
#[derive(Debug, Clone)]
pub struct PublishArgs {
    /// JSON file containing the candidate `hosts` map. Every leaf is an immutable store
    /// target; this command never evaluates an installable or accepts a mutable flake ref.
    pub targets_file: PathBuf,
    /// Existing complete manifest to preserve during a partial publication. Required as
    /// soon as either selector is present, so updating one target cannot delete every
    /// unselected target from the published document.
    pub base_manifest: Option<PathBuf>,
    /// Host-axis selectors. With no plane selectors, every supplied plane for each host is
    /// replaced; otherwise only matching plane names are.
    pub hosts: BTreeSet<String>,
    /// Plane-name selectors. When host selectors are also present, the two axes intersect.
    pub planes: BTreeSet<String>,
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
    /// The exact targets replaced in this publication. An empty successful publication is
    /// impossible.
    pub updated: Vec<PublishedTarget>,
    pub total_targets: usize,
    /// Non-fatal things an operator should see. Kept as data rather than printed from deep
    /// inside the call so a test can assert on them without capturing stderr.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedTarget {
    pub host: String,
    pub plane: String,
    pub target: String,
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
    let built_at = args
        .built_at
        .clone()
        .unwrap_or_else(|| iso8601_utc(now_unix));

    let targets_text = fs::read_to_string(&args.targets_file)
        .map_err(|e| PublishError::Read(args.targets_file.clone(), e.to_string()))?;
    let candidates: BTreeMap<String, HostEntry> = serde_json::from_str(&targets_text)
        .map_err(|e| PublishError::Parse(args.targets_file.clone(), e.to_string()))?;

    let partial = !args.hosts.is_empty() || !args.planes.is_empty();
    if partial && args.base_manifest.is_none() {
        return Err(PublishError::Usage(
            "--base-manifest is required with --host or --plane: a partial publish must preserve every unselected target"
                .to_string(),
        ));
    }
    if !partial && args.base_manifest.is_some() {
        return Err(PublishError::Usage(
            "--base-manifest is only meaningful with at least one --host or --plane selector"
                .to_string(),
        ));
    }

    let selected = select_targets(&candidates, &args.hosts, &args.planes)?;
    let selected_hosts = hosts_from_selection(&candidates, &selected);

    let mut selected_doc = ManifestDoc {
        version: SUPPORTED_SCHEMA_VERSION,
        revision: args.revision.clone(),
        built_at: built_at.clone(),
        hosts: selected_hosts,
    };
    let mut problems = validate(&selected_doc);

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

    if let Some(base_path) = &args.base_manifest {
        let base_text = fs::read_to_string(base_path)
            .map_err(|e| PublishError::Read(base_path.clone(), e.to_string()))?;
        let base_signature_path = signature_path(base_path);
        let base_signature = fs::read_to_string(&base_signature_path)
            .map_err(|e| PublishError::Read(base_signature_path.clone(), e.to_string()))?;
        verify_with_signing_key(&signing_key, base_text.as_bytes(), base_signature.trim())
            .map_err(|error| {
                PublishError::Invalid(vec![format!(
                    "base manifest signature does not verify with --signing-key-file: {}",
                    error
                )])
            })?;
        let base: ManifestDoc = serde_json::from_str(&base_text)
            .map_err(|e| PublishError::Parse(base_path.clone(), e.to_string()))?;
        if base.version != SUPPORTED_SCHEMA_VERSION {
            return Err(PublishError::Invalid(vec![format!(
                "base manifest schema version {} is not {}",
                base.version, SUPPORTED_SCHEMA_VERSION
            )]));
        }
        problems = validate(&base);
        if !problems.is_empty() {
            return Err(PublishError::Invalid(
                problems
                    .into_iter()
                    .map(|problem| format!("base manifest: {}", problem))
                    .collect(),
            ));
        }

        let mut hosts = base.hosts;
        for (host_name, plane_name) in &selected {
            let plane = candidates[host_name].planes[plane_name].clone();
            hosts
                .entry(host_name.clone())
                .or_insert_with(|| HostEntry {
                    planes: BTreeMap::new(),
                })
                .planes
                .insert(plane_name.clone(), plane);
        }
        selected_doc.hosts = hosts;
    }

    let doc = selected_doc;
    problems = validate(&doc);
    if !problems.is_empty() {
        return Err(PublishError::Invalid(problems));
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
        updated: selected
            .into_iter()
            .map(|(host, plane)| PublishedTarget {
                target: doc.hosts[&host].planes[&plane].target.clone(),
                host,
                plane,
            })
            .collect(),
        total_targets: doc.hosts.values().map(|host| host.planes.len()).sum(),
        warnings,
    })
}

fn select_targets(
    candidates: &BTreeMap<String, HostEntry>,
    hosts: &BTreeSet<String>,
    planes: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>, PublishError> {
    let mut selected = BTreeSet::new();
    let mut problems = Vec::new();

    for host_name in hosts {
        if !candidates.contains_key(host_name) {
            problems.push(format!("--host {:?} has no entry in --targets", host_name));
        }
    }
    for plane_name in planes {
        if !candidates
            .values()
            .any(|host| host.planes.contains_key(plane_name))
        {
            problems.push(format!(
                "--plane {:?} has no entry under any host in --targets",
                plane_name
            ));
        }
    }

    for (host_name, host) in candidates {
        if !hosts.is_empty() && !hosts.contains(host_name) {
            continue;
        }
        for plane_name in host.planes.keys() {
            if planes.is_empty() || planes.contains(plane_name) {
                selected.insert((host_name.clone(), plane_name.clone()));
            }
        }
    }

    if selected.is_empty() {
        problems.push("selection names no target".to_string());
    }
    if problems.is_empty() {
        Ok(selected)
    } else {
        Err(PublishError::Invalid(problems))
    }
}

fn hosts_from_selection(
    candidates: &BTreeMap<String, HostEntry>,
    selected: &BTreeSet<(String, String)>,
) -> BTreeMap<String, HostEntry> {
    let mut hosts = BTreeMap::new();
    for (host_name, plane_name) in selected {
        let plane: PlaneEntry = candidates[host_name].planes[plane_name].clone();
        hosts
            .entry(host_name.clone())
            .or_insert_with(|| HostEntry {
                planes: BTreeMap::new(),
            })
            .planes
            .insert(plane_name.clone(), plane);
    }
    hosts
}

/// Where the detached signature for a manifest at `out` goes: `<out>.sig`, the sibling
/// `manifest::fetch_and_verify` derives from the manifest URL the same way.
pub fn signature_path(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".sig");
    PathBuf::from(name)
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
    let mut targets_file: Option<PathBuf> = None;
    let mut base_manifest: Option<PathBuf> = None;
    let mut hosts = BTreeSet::new();
    let mut planes = BTreeSet::new();
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
            "--targets" => {
                targets_file = Some(PathBuf::from(value_of(args, &mut i, "--targets", inline)?))
            }
            "--base-manifest" => {
                base_manifest = Some(PathBuf::from(value_of(
                    args,
                    &mut i,
                    "--base-manifest",
                    inline,
                )?))
            }
            "--host" => {
                hosts.insert(value_of(args, &mut i, "--host", inline)?);
            }
            "--plane" => {
                let value = value_of(args, &mut i, "--plane", inline)?;
                if !["nixos", "system-manager", "home-manager", "nix-darwin"]
                    .contains(&value.as_str())
                {
                    return Err(PublishError::Usage(format!(
                        "--plane {:?} is not one of: nixos, system-manager, home-manager, nix-darwin",
                        value
                    )));
                }
                planes.insert(value);
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
        targets_file: targets_file
            .ok_or_else(|| PublishError::Usage("--targets is required".to_string()))?,
        base_manifest,
        hosts,
        planes,
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
nixdeploy publish --targets FILE --revision REV --signing-key-file FILE --out FILE [--built-at TS]
                  [--base-manifest FILE] [--host HOST...] [--plane PLANE...]

  --targets FILE           Candidate JSON `hosts` map. Each host contains named `planes`;
                           each plane names a backend and exact Nix store `target`.
  --base-manifest FILE     Existing complete v2 manifest. Required for a partial publish.
  --host HOST              Replace every candidate plane for HOST. Repeatable.
  --plane PLANE            Restrict plane names. Repeatable. With --host, both axes
                           intersect over the candidate host-to-planes map.
  --revision REV           Opaque build revision recorded in the manifest.
  --signing-key-file FILE  ed25519 secret key, in `nix-store --generate-binary-cache-key`
                           format. A file only: never a flag value, never an environment
                           variable.
  --out FILE               Manifest path. The detached signature is written to FILE.sig.
  --built-at TS            ISO-8601 UTC, e.g. 2026-08-03T12:00:00Z. Defaults to now.

With no selectors, FILE replaces the entire manifest and must be complete. With selectors,
unselected targets are copied from --base-manifest so a granular update cannot remove them.
Builds nothing and uploads nothing: every target must already exist in a trusted cache.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{looks_like_store_path, verify_and_select, ManifestError};
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use ed25519_dalek::SigningKey;

    const PATH_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system-host-a";
    const PATH_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-system-host-b";
    const PATH_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-system-new";

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
        let targets_file = dir.join("targets.json");
        fs::write(&targets_file, hosts_json).expect("write targets");
        PublishArgs {
            targets_file,
            base_manifest: None,
            hosts: BTreeSet::new(),
            planes: BTreeSet::new(),
            revision: "rev-1".to_string(),
            built_at: Some("2026-08-03T12:00:00Z".to_string()),
            signing_key_file: key.to_path_buf(),
            out: dir.join("manifest.json"),
        }
    }

    fn two_hosts() -> String {
        format!(
            r#"{{"host-b":{{"planes":{{"nixos":{{"backend":"nixos","target":"{}","image":"image-b"}}}}}},
                 "host-a":{{"planes":{{"nixos":{{"backend":"nixos","target":"{}"}},"home-manager":{{"backend":"home-manager","identity":"alice","target":"{}"}}}}}}}}"#,
            PATH_B, PATH_A, PATH_A
        )
    }

    #[test]
    fn published_manifest_verifies_as_the_bytes_on_disk() {
        let dir = tmpdir("roundtrip");
        let (key, public) = write_key(&dir, [11u8; 32]);
        let args = args_for(&dir, &two_hosts(), &key);

        let published = publish(&args, 0).expect("publish");
        assert_eq!(published.updated.len(), 3);
        assert_eq!(published.total_targets, 3);
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
        let target =
            verify_and_select(&body, sig.trim(), &public, "host-a", "nixos").expect("verify");
        assert_eq!(target.store_path, PATH_A);
        assert_eq!(target.image, None, "host-a declared no image");

        let target_b =
            verify_and_select(&body, sig.trim(), &public, "host-b", "nixos").expect("verify");
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
            let result = verify_and_select(&mutated, sig.trim(), &public, "host-a", "nixos");
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
            r#"{{"host-a":{{"planes":{{"home-manager":{{"target":"{}","identity":"alice","backend":"home-manager"}},"nixos":{{"target":"{}","backend":"nixos"}}}}}},
                 "host-b":{{"planes":{{"nixos":{{"image":"image-b","target":"{}","backend":"nixos"}}}}}}}}"#,
            PATH_A, PATH_A, PATH_B
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
            r#"{{"host-a":{{"planes":{{"nixos":{{"backend":"nixos","target":"not-a-store-path"}}}}}},
                 "host-b":{{"planes":{{"home-manager":{{"backend":"home-manager","target":"{}"}}}}}},
                 "host-c":{{"planes":{{"system-manager":{{"backend":"system-manager","target":"{}","image":"wrong"}}}}}}}}"#,
            PATH_B, PATH_A
        );
        let mut args = args_for(&dir, &hosts, &key);
        args.revision = "  ".to_string();

        let err = publish(&args, 0).unwrap_err();
        let PublishError::Invalid(problems) = &err else {
            panic!("want Invalid, got {:?}", err);
        };
        let joined = problems.join("\n");
        for expected in [
            "does not look like a Nix store path",
            "identity",
            "only meaningful for the nixos backend",
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
            problems.iter().any(|p| p.contains("no target")),
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
            "--targets=/tmp/targets.json".to_string(),
            "--revision".to_string(),
            "abc".to_string(),
            "--signing-key-file".to_string(),
            "/tmp/key".to_string(),
            "--out=/tmp/manifest.json".to_string(),
        ])
        .expect("parse");
        assert_eq!(args.targets_file, PathBuf::from("/tmp/targets.json"));
        assert_eq!(args.revision, "abc");
        assert_eq!(args.out, PathBuf::from("/tmp/manifest.json"));
        assert_eq!(args.built_at, None);

        let selected = parse_args(&[
            "--targets=/tmp/targets.json".to_string(),
            "--base-manifest=/tmp/base.json".to_string(),
            "--host=host-a".to_string(),
            "--host".to_string(),
            "host-b".to_string(),
            "--plane=nixos".to_string(),
            "--revision=r".to_string(),
            "--signing-key-file=/tmp/key".to_string(),
            "--out=/tmp/manifest.json".to_string(),
        ])
        .expect("parse selectors");
        assert_eq!(
            selected.hosts,
            ["host-a".to_string(), "host-b".to_string()]
                .into_iter()
                .collect()
        );
        assert_eq!(selected.planes, ["nixos".to_string()].into_iter().collect());

        // A typo'd flag must not be silently ignored: `--build-at` falling back to "now"
        // would publish a timestamp nobody asked for.
        let err = parse_args(&[
            "--targets".to_string(),
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

        let missing = parse_args(&["--targets".to_string(), "/tmp/h".to_string()]).unwrap_err();
        assert!(matches!(missing, PublishError::Usage(_)));
    }

    #[test]
    fn host_and_plane_selectors_intersect_and_preserve_every_other_target() {
        let dir = tmpdir("partial-intersection");
        let (key, public) = write_key(&dir, [18u8; 32]);

        let mut base_args = args_for(&dir, &two_hosts(), &key);
        base_args.out = dir.join("base.json");
        publish(&base_args, 0).expect("publish base");

        let candidates = format!(
            r#"{{"host-a":{{"planes":{{"nixos":{{"backend":"nixos","target":"{}"}},"home-manager":{{"backend":"home-manager","identity":"alice","target":"{}"}}}}}},"host-b":{{"planes":{{"nixos":{{"backend":"nixos","target":"{}","image":"new-image"}}}}}}}}"#,
            PATH_C, PATH_B, PATH_C
        );
        let mut args = args_for(&dir, &candidates, &key);
        args.base_manifest = Some(base_args.out.clone());
        args.hosts.insert("host-a".to_string());
        args.planes.insert("nixos".to_string());
        args.out = dir.join("partial.json");

        let published = publish(&args, 0).expect("partial publish");
        assert_eq!(
            published.updated,
            vec![PublishedTarget {
                host: "host-a".to_string(),
                plane: "nixos".to_string(),
                target: PATH_C.to_string(),
            }]
        );
        assert_eq!(published.total_targets, 3);

        let body = fs::read_to_string(&published.manifest).unwrap();
        let sig = fs::read_to_string(&published.signature).unwrap();
        assert_eq!(
            verify_and_select(&body, sig.trim(), &public, "host-a", "nixos")
                .unwrap()
                .store_path,
            PATH_C
        );
        assert_eq!(
            verify_and_select(&body, sig.trim(), &public, "host-a", "home-manager")
                .unwrap()
                .store_path,
            PATH_A,
            "the unselected home plane must survive from the base manifest"
        );
        assert_eq!(
            verify_and_select(&body, sig.trim(), &public, "host-b", "nixos")
                .unwrap()
                .store_path,
            PATH_B,
            "the unselected host must survive from the base manifest"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_selection_requires_a_base_manifest() {
        let dir = tmpdir("partial-needs-base");
        let (key, _public) = write_key(&dir, [19u8; 32]);
        let mut args = args_for(&dir, &two_hosts(), &key);
        args.planes.insert("nixos".to_string());

        let error = publish(&args, 0).unwrap_err();
        assert!(matches!(error, PublishError::Usage(_)), "got {:?}", error);
        assert!(!args.out.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_publish_never_resigns_a_tampered_base() {
        let dir = tmpdir("tampered-base");
        let (key, _public) = write_key(&dir, [20u8; 32]);
        let mut base_args = args_for(&dir, &two_hosts(), &key);
        base_args.out = dir.join("base.json");
        publish(&base_args, 0).expect("publish base");
        let base_body = fs::read_to_string(&base_args.out).unwrap();
        fs::write(&base_args.out, base_body.replace(PATH_B, PATH_C)).unwrap();

        let mut args = args_for(&dir, &two_hosts(), &key);
        args.base_manifest = Some(base_args.out.clone());
        args.hosts.insert("host-a".to_string());
        args.out = dir.join("next.json");

        let error = publish(&args, 0).unwrap_err();
        let PublishError::Invalid(problems) = error else {
            panic!("want invalid base signature");
        };
        assert!(problems.iter().any(|problem| problem.contains("signature")));
        assert!(!args.out.exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn signature_path_is_the_sibling_the_receiver_looks_for() {
        assert_eq!(
            signature_path(Path::new("/srv/www/manifest.json")),
            PathBuf::from("/srv/www/manifest.json.sig")
        );
    }
}
