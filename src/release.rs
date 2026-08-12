//! Content-addressed deployment sets and their transactional promotion.
//!
//! A Git revision is provenance for one artifact, not the identity of a multi-host release.
//! A deployment-set ID is therefore the SHA-256 of the canonical desired-state composition:
//! every selected host plane, its immutable artifact, its exact source/lock provenance, and
//! the compatibility requirements a receiver must satisfy before activation.
//!
//! The stable channel is one signed JSON envelope. The signed payload is base64 so the exact
//! verified bytes are unambiguous, while keeping signature and payload in one atomically
//! replaced file. Promotion writes an immutable release and signed journal record before it
//! moves the channel. A later invocation repairs a channel from that journal rather than
//! replaying an old publisher binary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::SigningKey;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomicfile::write_atomic;
use crate::manifest::{
    looks_like_store_path, parse_public_key, sign_detached, verify, verify_with_signing_key,
    Backend,
};

pub const DEPLOYMENT_SET_VERSION: u32 = 4;
pub const ENVELOPE_VERSION: u32 = 1;
pub const PROMOTION_RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeploymentSet {
    pub schema_version: u32,
    pub hosts: BTreeMap<String, ReleaseHostEntry>,
}

impl DeploymentSet {
    pub fn new(hosts: BTreeMap<String, ReleaseHostEntry>) -> Self {
        Self {
            schema_version: DEPLOYMENT_SET_VERSION,
            hosts,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("DeploymentSet contains only serializable fields")
    }

    pub fn id(&self) -> String {
        digest_id(&self.canonical_bytes())
    }

    pub fn validate(&self) -> Vec<String> {
        validate_set(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseHostEntry {
    pub planes: BTreeMap<String, ReleasePlaneEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReleasePlaneEntry {
    pub backend: Backend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    pub artifact: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<ReleaseBootSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Artifact {
    pub target: String,
    pub nar_hash: String,
    /// Digest of the sorted `(store path, narHash, references)` records for the entire
    /// transitive closure. `narHash` identifies only the root NAR; this identifies the exact
    /// cache-complete closure receivers must be able to substitute.
    pub closure_digest: String,
    pub provenance: ArtifactProvenance,
    pub requirements: ArtifactRequirements,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProvenance {
    pub source: SourceProvenance,
    pub builder: BuilderProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceProvenance {
    pub repository: String,
    pub revision: String,
    pub lock_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BuilderProvenance {
    /// Stable logical builder identity, not a transient pod name.
    pub id: String,
    /// Full client banner, retained as evidence (including Determinate branding).
    pub nix_version: String,
    /// Store daemon implementation version, used for compatibility decisions.
    pub store_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ArtifactRequirements {
    pub system: String,
    pub minimum_store_version: String,
}

impl ArtifactRequirements {
    /// Determinate and upstream clients can have different banners while speaking to the
    /// same compatible store daemon. Compatibility therefore uses the declared host system
    /// and daemon version, never the Nix distribution name.
    pub fn check(&self, system: &str, store_version: &str) -> Result<(), CompatibilityError> {
        if self.system != system {
            return Err(CompatibilityError::System {
                required: self.system.clone(),
                actual: system.to_string(),
            });
        }
        let required = parse_version(&self.minimum_store_version).ok_or_else(|| {
            CompatibilityError::InvalidRequiredVersion(self.minimum_store_version.clone())
        })?;
        let actual = parse_version(store_version)
            .ok_or_else(|| CompatibilityError::UnknownStoreVersion(store_version.to_string()))?;
        if version_less_than(&actual, &required) {
            return Err(CompatibilityError::StoreVersion {
                required: self.minimum_store_version.clone(),
                actual: store_version.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityError {
    System { required: String, actual: String },
    StoreVersion { required: String, actual: String },
    InvalidRequiredVersion(String),
    UnknownStoreVersion(String),
}

impl fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompatibilityError::System { required, actual } => write!(
                f,
                "artifact requires system {}, receiver is {}",
                required, actual
            ),
            CompatibilityError::StoreVersion { required, actual } => write!(
                f,
                "artifact requires store daemon >= {}, receiver has {}",
                required, actual
            ),
            CompatibilityError::InvalidRequiredVersion(v) => {
                write!(f, "artifact has invalid minimum store version {:?}", v)
            }
            CompatibilityError::UnknownStoreVersion(v) => {
                write!(f, "receiver store daemon version {:?} is not comparable", v)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ReleaseBootSpec {
    None,
    Managed { roles: ReleaseBootRoles },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseBootRoles {
    pub primary: ReleaseBootArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixrescue: Option<ReleaseBootArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseBootArtifact {
    pub artifact: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReleaseDocument {
    pub deployment_set_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_deployment_set_id: Option<String>,
    pub published_at: String,
    pub deployment_set: DeploymentSet,
}

impl ReleaseDocument {
    pub fn validate(&self) -> Vec<String> {
        let mut problems = self.deployment_set.validate();
        let computed = self.deployment_set.id();
        if self.deployment_set_id != computed {
            problems.push(format!(
                "deploymentSetId {:?} does not match deployment set contents ({})",
                self.deployment_set_id, computed
            ));
        }
        if let Some(parent) = &self.parent_deployment_set_id {
            if !looks_like_id(parent) {
                problems.push(format!(
                    "parentDeploymentSetId {:?} is not a sha256 deployment-set ID",
                    parent
                ));
            }
        }
        if !looks_like_timestamp(&self.published_at) {
            problems.push(format!(
                "publishedAt {:?} is not an ISO-8601 UTC timestamp",
                self.published_at
            ));
        }
        problems
    }
}

/// One-file signature envelope. The signature covers the decoded payload bytes exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SignedEnvelope {
    pub envelope_version: u32,
    pub payload: String,
    pub signature: String,
}

impl SignedEnvelope {
    pub fn seal<T: Serialize>(value: &T, key_name: &str, key: &SigningKey) -> Self {
        let payload = serde_json::to_vec(value).expect("signed payload must serialize");
        Self {
            envelope_version: ENVELOPE_VERSION,
            payload: BASE64.encode(&payload),
            signature: sign_detached(key_name, key, &payload),
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("SignedEnvelope contains only strings and an integer")
    }

    pub fn parse(text: &[u8]) -> Result<Self, String> {
        let envelope: Self =
            serde_json::from_slice(text).map_err(|e| format!("parsing signed envelope: {}", e))?;
        if envelope.envelope_version != ENVELOPE_VERSION {
            return Err(format!(
                "signed envelope version {} is not supported (want {})",
                envelope.envelope_version, ENVELOPE_VERSION
            ));
        }
        Ok(envelope)
    }

    pub fn open<T: DeserializeOwned>(&self, public_key: &str) -> Result<T, String> {
        let payload = self.payload_bytes()?;
        let key = parse_public_key(public_key).map_err(|e| e.to_string())?;
        verify(&key, &payload, &self.signature).map_err(|e| e.to_string())?;
        serde_json::from_slice(&payload).map_err(|e| format!("parsing verified payload: {}", e))
    }

    pub fn open_with_signing_key<T: DeserializeOwned>(
        &self,
        key: &SigningKey,
    ) -> Result<T, String> {
        let payload = self.payload_bytes()?;
        verify_with_signing_key(key, &payload, &self.signature).map_err(|e| e.to_string())?;
        serde_json::from_slice(&payload).map_err(|e| format!("parsing verified payload: {}", e))
    }

    fn payload_bytes(&self) -> Result<Vec<u8>, String> {
        BASE64
            .decode(&self.payload)
            .map_err(|e| format!("signed envelope payload is not valid base64: {}", e))
    }
}

pub fn sign_release(
    deployment_set: DeploymentSet,
    parent_deployment_set_id: Option<String>,
    published_at: String,
    key_name: &str,
    key: &SigningKey,
) -> Result<SignedEnvelope, Vec<String>> {
    let doc = ReleaseDocument {
        deployment_set_id: deployment_set.id(),
        parent_deployment_set_id,
        published_at,
        deployment_set,
    };
    let problems = doc.validate();
    if problems.is_empty() {
        Ok(SignedEnvelope::seal(&doc, key_name, key))
    } else {
        Err(problems)
    }
}

pub fn verify_release(bytes: &[u8], public_key: &str) -> Result<ReleaseDocument, String> {
    let envelope = SignedEnvelope::parse(bytes)?;
    let doc: ReleaseDocument = envelope.open(public_key)?;
    verify_release_document(doc)
}

fn verify_release_with_key(bytes: &[u8], key: &SigningKey) -> Result<ReleaseDocument, String> {
    let envelope = SignedEnvelope::parse(bytes)?;
    let doc: ReleaseDocument = envelope.open_with_signing_key(key)?;
    verify_release_document(doc)
}

fn verify_release_document(doc: ReleaseDocument) -> Result<ReleaseDocument, String> {
    let problems = doc.validate();
    if problems.is_empty() {
        Ok(doc)
    } else {
        Err(format!("invalid verified release: {}", problems.join("; ")))
    }
}

#[derive(Debug, Clone)]
pub struct PromotionRequest {
    pub candidates: BTreeMap<String, ReleaseHostEntry>,
    pub hosts: BTreeSet<String>,
    pub planes: BTreeSet<String>,
    /// Compare-and-swap base. `None` means the origin must not have a stable release yet.
    pub expected_base: Option<String>,
    pub published_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromotionStatus {
    Promoted,
    Unchanged,
    Superseded,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionOutcome {
    pub status: PromotionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_set_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_deployment_set_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug)]
pub enum PromotionError {
    Io(PathBuf, String),
    Trust(PathBuf, String),
    Lock(PathBuf, String),
}

impl fmt::Display for PromotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromotionError::Io(path, error) => write!(f, "{}: {}", path.display(), error),
            PromotionError::Trust(path, error) => {
                write!(f, "refusing untrusted {}: {}", path.display(), error)
            }
            PromotionError::Lock(path, error) => {
                write!(f, "locking {}: {}", path.display(), error)
            }
        }
    }
}

impl std::error::Error for PromotionError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PromotionRecord {
    version: u32,
    generation: u64,
    deployment_set_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_deployment_set_id: Option<String>,
    release_digest: String,
    promoted_at: String,
}

pub struct ReleaseStore {
    root: PathBuf,
}

impl ReleaseStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn promote(
        &self,
        request: &PromotionRequest,
        key_name: &str,
        key: &SigningKey,
    ) -> Result<PromotionOutcome, PromotionError> {
        self.prepare()?;
        let _lock = PromotionLock::acquire(&self.root.join(".promotion.lock"))?;
        self.recover_locked(key)?;

        let candidate_set = DeploymentSet::new(request.candidates.clone());
        let mut problems = candidate_set.validate();
        if !looks_like_timestamp(&request.published_at) {
            problems.push(format!(
                "publishedAt {:?} is not an ISO-8601 UTC timestamp",
                request.published_at
            ));
        }
        if let Some(expected) = &request.expected_base {
            if !looks_like_id(expected) {
                problems.push(format!(
                    "expected base {:?} is not a sha256 deployment-set ID",
                    expected
                ));
            }
        }
        let selected = match select_targets(&request.candidates, &request.hosts, &request.planes) {
            Ok(selected) => selected,
            Err(selection_problems) => {
                problems.extend(selection_problems);
                BTreeSet::new()
            }
        };
        if !problems.is_empty() {
            return Ok(PromotionOutcome {
                status: PromotionStatus::Rejected,
                deployment_set_id: None,
                previous_deployment_set_id: self.current_id_with_key(key)?,
                generation: None,
                details: problems,
            });
        }

        let current = self.read_current_with_key(key)?;
        let current_id = current
            .as_ref()
            .map(|(_, doc)| doc.deployment_set_id.clone());

        // A retry after the journal landed and recovery moved the channel is success, even
        // though its old expected base is now stale. This is the only stale-CAS exception.
        if selection_already_present(
            current.as_ref().map(|(_, d)| &d.deployment_set),
            &candidate_set,
            &selected,
            request.hosts.is_empty() && request.planes.is_empty(),
        ) {
            return Ok(PromotionOutcome {
                status: PromotionStatus::Unchanged,
                deployment_set_id: current_id.clone(),
                previous_deployment_set_id: current_id,
                generation: self.latest_record_with_key(key)?.map(|r| r.generation),
                details: vec!["candidate is already the stable deployment set".to_string()],
            });
        }

        if current_id != request.expected_base {
            return Ok(PromotionOutcome {
                status: PromotionStatus::Superseded,
                deployment_set_id: None,
                previous_deployment_set_id: current_id.clone(),
                generation: None,
                details: vec![format!(
                    "expected base {}, stable channel is {}",
                    display_optional_id(request.expected_base.as_deref()),
                    display_optional_id(current_id.as_deref())
                )],
            });
        }

        let full = request.hosts.is_empty() && request.planes.is_empty();
        let deployment_set = if full {
            candidate_set
        } else {
            let Some((_, current_doc)) = &current else {
                return Ok(PromotionOutcome {
                    status: PromotionStatus::Rejected,
                    deployment_set_id: None,
                    previous_deployment_set_id: None,
                    generation: None,
                    details: vec![
                        "partial promotion requires an existing stable deployment set".to_string(),
                    ],
                });
            };
            compose_selected(&current_doc.deployment_set, &candidate_set, &selected)
        };

        let envelope = sign_release(
            deployment_set,
            current_id.clone(),
            request.published_at.clone(),
            key_name,
            key,
        )
        .map_err(|problems| {
            PromotionError::Trust(
                self.root.clone(),
                format!("composed release: {}", problems.join("; ")),
            )
        })?;
        let proposed_bytes = envelope.canonical_bytes();
        let proposed_doc = verify_release_with_key(&proposed_bytes, key)
            .map_err(|e| PromotionError::Trust(self.root.clone(), e))?;
        let release_path = self.release_path(&proposed_doc.deployment_set_id);

        let release_bytes = if release_path.exists() {
            let existing = read(&release_path)?;
            let existing_doc = verify_release_with_key(&existing, key)
                .map_err(|e| PromotionError::Trust(release_path.clone(), e))?;
            if existing_doc.deployment_set_id != proposed_doc.deployment_set_id {
                return Err(PromotionError::Trust(
                    release_path,
                    "file name and signed deployment-set ID disagree".to_string(),
                ));
            }
            existing
        } else {
            write_immutable(&release_path, &proposed_bytes, 0o644)?;
            proposed_bytes
        };

        let generation = self
            .latest_record_with_key(key)?
            .map(|record| record.generation + 1)
            .unwrap_or(1);
        let record = PromotionRecord {
            version: PROMOTION_RECORD_VERSION,
            generation,
            deployment_set_id: proposed_doc.deployment_set_id.clone(),
            previous_deployment_set_id: current_id.clone(),
            release_digest: digest_id(&release_bytes),
            promoted_at: request.published_at.clone(),
        };
        let record_bytes = SignedEnvelope::seal(&record, key_name, key).canonical_bytes();
        let record_path = self.record_path(generation, &proposed_doc.deployment_set_id);
        write_immutable(&record_path, &record_bytes, 0o644)?;

        let channel = self.channel_path();
        write_atomic(&channel, &release_bytes, 0o644)
            .map_err(|e| PromotionError::Io(channel.clone(), e))?;
        sync_dir(channel.parent().expect("channel has parent"))?;

        Ok(PromotionOutcome {
            status: PromotionStatus::Promoted,
            deployment_set_id: Some(proposed_doc.deployment_set_id),
            previous_deployment_set_id: current_id,
            generation: Some(generation),
            details: Vec::new(),
        })
    }

    pub fn recover(&self, key: &SigningKey) -> Result<Option<String>, PromotionError> {
        self.prepare()?;
        let _lock = PromotionLock::acquire(&self.root.join(".promotion.lock"))?;
        self.recover_locked(key)
    }

    fn prepare(&self) -> Result<(), PromotionError> {
        for path in [
            self.root.clone(),
            self.root.join("releases"),
            self.root.join("promotions"),
            self.root.join("channels"),
        ] {
            fs::create_dir_all(&path)
                .map_err(|e| PromotionError::Io(path.clone(), e.to_string()))?;
        }
        Ok(())
    }

    fn recover_locked(&self, key: &SigningKey) -> Result<Option<String>, PromotionError> {
        let Some(record) = self.latest_record_with_key(key)? else {
            return Ok(None);
        };
        let release_path = self.release_path(&record.deployment_set_id);
        let release_bytes = read(&release_path)?;
        if digest_id(&release_bytes) != record.release_digest {
            return Err(PromotionError::Trust(
                release_path,
                "immutable release bytes do not match signed promotion record".to_string(),
            ));
        }
        let doc = verify_release_with_key(&release_bytes, key)
            .map_err(|e| PromotionError::Trust(release_path.clone(), e))?;
        if doc.deployment_set_id != record.deployment_set_id {
            return Err(PromotionError::Trust(
                release_path,
                "signed release and promotion record name different deployment sets".to_string(),
            ));
        }
        if doc.parent_deployment_set_id != record.previous_deployment_set_id {
            return Err(PromotionError::Trust(
                release_path,
                "signed release parent and promotion record previous ID disagree".to_string(),
            ));
        }

        let channel = self.channel_path();
        let needs_repair = match fs::read(&channel) {
            Ok(current) => current != release_bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(PromotionError::Io(channel.clone(), error.to_string())),
        };
        if needs_repair {
            write_atomic(&channel, &release_bytes, 0o644)
                .map_err(|e| PromotionError::Io(channel.clone(), e))?;
            sync_dir(channel.parent().expect("channel has parent"))?;
        }
        Ok(Some(record.deployment_set_id))
    }

    fn current_id_with_key(&self, key: &SigningKey) -> Result<Option<String>, PromotionError> {
        Ok(self
            .read_current_with_key(key)?
            .map(|(_, document)| document.deployment_set_id))
    }

    fn read_current_with_key(
        &self,
        key: &SigningKey,
    ) -> Result<Option<(Vec<u8>, ReleaseDocument)>, PromotionError> {
        let path = self.channel_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PromotionError::Io(path.clone(), error.to_string())),
        };
        let doc = verify_release_with_key(&bytes, key)
            .map_err(|e| PromotionError::Trust(path.clone(), e))?;
        Ok(Some((bytes, doc)))
    }

    fn latest_record_with_key(
        &self,
        key: &SigningKey,
    ) -> Result<Option<PromotionRecord>, PromotionError> {
        let dir = self.root.join("promotions");
        let mut paths = fs::read_dir(&dir)
            .map_err(|e| PromotionError::Io(dir.clone(), e.to_string()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        paths.sort();

        let mut latest: Option<PromotionRecord> = None;
        for path in paths {
            let bytes = read(&path)?;
            let envelope = SignedEnvelope::parse(&bytes)
                .map_err(|e| PromotionError::Trust(path.clone(), e))?;
            let record: PromotionRecord = envelope
                .open_with_signing_key(key)
                .map_err(|e| PromotionError::Trust(path.clone(), e))?;
            if record.version != PROMOTION_RECORD_VERSION {
                return Err(PromotionError::Trust(
                    path,
                    format!(
                        "promotion record version {} is not {}",
                        record.version, PROMOTION_RECORD_VERSION
                    ),
                ));
            }
            if !looks_like_id(&record.deployment_set_id)
                || !looks_like_id(&record.release_digest)
                || !looks_like_timestamp(&record.promoted_at)
            {
                return Err(PromotionError::Trust(
                    path,
                    "promotion record contains malformed identity or timestamp".to_string(),
                ));
            }
            let expected_path = self.record_path(record.generation, &record.deployment_set_id);
            if path != expected_path {
                return Err(PromotionError::Trust(
                    path,
                    format!(
                        "signed record identity requires filename {}",
                        expected_path.display()
                    ),
                ));
            }
            match &latest {
                None => {
                    if record.generation != 1 || record.previous_deployment_set_id.is_some() {
                        return Err(PromotionError::Trust(
                            path,
                            "first promotion must be generation 1 with no previous ID".to_string(),
                        ));
                    }
                }
                Some(previous) => {
                    if record.generation != previous.generation + 1 {
                        return Err(PromotionError::Trust(
                            path,
                            "promotion generations must be contiguous".to_string(),
                        ));
                    }
                    if record.previous_deployment_set_id.as_deref()
                        != Some(previous.deployment_set_id.as_str())
                    {
                        return Err(PromotionError::Trust(
                            path,
                            "promotion previous ID does not continue the signed journal"
                                .to_string(),
                        ));
                    }
                }
            }
            latest = Some(record);
        }
        Ok(latest)
    }

    fn channel_path(&self) -> PathBuf {
        self.root.join("channels/stable.json")
    }

    fn release_path(&self, id: &str) -> PathBuf {
        self.root
            .join("releases")
            .join(format!("{}.json", id.trim_start_matches("sha256:")))
    }

    fn record_path(&self, generation: u64, id: &str) -> PathBuf {
        self.root.join("promotions").join(format!(
            "{:020}-{}.json",
            generation,
            id.trim_start_matches("sha256:")
        ))
    }
}

struct PromotionLock {
    file: fs::File,
}

impl PromotionLock {
    fn acquire(path: &Path) -> Result<Self, PromotionError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| PromotionError::Lock(path.to_path_buf(), e.to_string()))?;
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if status != 0 {
            return Err(PromotionError::Lock(
                path.to_path_buf(),
                std::io::Error::last_os_error().to_string(),
            ));
        }
        Ok(Self { file })
    }
}

impl Drop for PromotionLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn validate_set(set: &DeploymentSet) -> Vec<String> {
    let mut problems = Vec::new();
    if set.schema_version != DEPLOYMENT_SET_VERSION {
        problems.push(format!(
            "schemaVersion {} is not {}",
            set.schema_version, DEPLOYMENT_SET_VERSION
        ));
    }
    if set.hosts.is_empty() {
        problems.push("hosts must contain at least one host".to_string());
    }
    for (host_name, host) in &set.hosts {
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
                    "name must equal backend {:?}",
                    plane.backend.as_str()
                )));
            }
            match plane.backend {
                Backend::HomeManager => {
                    if plane
                        .identity
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .is_empty()
                    {
                        problems.push(prefix(
                            "identity is required and must be non-empty for home-manager",
                        ));
                    }
                }
                _ if plane.identity.is_some() => {
                    problems.push(prefix("identity is only valid for home-manager"));
                }
                _ => {}
            }
            validate_artifact(&plane.artifact, &prefix("artifact"), &mut problems);
            match (&plane.boot, plane.backend) {
                (None, Backend::Nixos) => {
                    problems.push(prefix("boot is required; use mode none when unmanaged"));
                }
                (Some(_), backend)
                    if !matches!(backend, Backend::Nixos | Backend::SystemManager) =>
                {
                    problems.push(prefix(
                        "boot is valid only for nixos and system-manager system planes",
                    ));
                }
                (
                    Some(ReleaseBootSpec::Managed { roles }),
                    Backend::Nixos | Backend::SystemManager,
                ) => {
                    validate_boot_artifact(
                        &roles.primary,
                        &prefix("boot role primary"),
                        &mut problems,
                    );
                    if let Some(rescue) = &roles.nixrescue {
                        validate_boot_artifact(
                            rescue,
                            &prefix("boot role nixrescue"),
                            &mut problems,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    problems
}

fn validate_boot_artifact(
    artifact: &ReleaseBootArtifact,
    prefix: &str,
    problems: &mut Vec<String>,
) {
    validate_artifact(&artifact.artifact, prefix, problems);
    if artifact.image.as_deref() == Some("") {
        problems.push(format!("{}: image must not be empty", prefix));
    }
}

fn validate_artifact(artifact: &Artifact, prefix: &str, problems: &mut Vec<String>) {
    if !looks_like_store_path(&artifact.target) {
        problems.push(format!(
            "{}: target does not look like a Nix store path",
            prefix
        ));
    }
    if !looks_like_nar_hash(&artifact.nar_hash) {
        problems.push(format!("{}: narHash is not an SRI sha256 hash", prefix));
    }
    if !looks_like_id(&artifact.closure_digest) {
        problems.push(format!("{}: closureDigest must be sha256:<hex>", prefix));
    }
    if artifact.provenance.source.repository.trim().is_empty() {
        problems.push(format!("{}: source repository must not be empty", prefix));
    }
    if !looks_like_git_revision(&artifact.provenance.source.revision) {
        problems.push(format!(
            "{}: source revision must be a full 40- or 64-digit Git object ID",
            prefix
        ));
    }
    if !looks_like_id(&artifact.provenance.source.lock_digest) {
        problems.push(format!(
            "{}: source lockDigest must be sha256:<hex>",
            prefix
        ));
    }
    if artifact.provenance.builder.id.trim().is_empty()
        || artifact.provenance.builder.nix_version.trim().is_empty()
    {
        problems.push(format!(
            "{}: builder id and nixVersion must not be empty",
            prefix
        ));
    }
    if parse_version(&artifact.provenance.builder.store_version).is_none() {
        problems.push(format!(
            "{}: builder storeVersion is not comparable",
            prefix
        ));
    }
    if artifact.requirements.system.trim().is_empty()
        || artifact
            .requirements
            .system
            .chars()
            .any(char::is_whitespace)
    {
        problems.push(format!("{}: required system is invalid", prefix));
    }
    if parse_version(&artifact.requirements.minimum_store_version).is_none() {
        problems.push(format!("{}: minimumStoreVersion is not comparable", prefix));
    }
}

fn select_targets(
    candidates: &BTreeMap<String, ReleaseHostEntry>,
    hosts: &BTreeSet<String>,
    planes: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>, Vec<String>> {
    let mut selected = BTreeSet::new();
    let mut problems = Vec::new();
    for host in hosts {
        if !candidates.contains_key(host) {
            problems.push(format!(
                "selected host {:?} is absent from candidates",
                host
            ));
        }
    }
    for plane in planes {
        if !candidates
            .values()
            .any(|host| host.planes.contains_key(plane))
        {
            problems.push(format!(
                "selected plane {:?} is absent from candidates",
                plane
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
        Err(problems)
    }
}

fn compose_selected(
    base: &DeploymentSet,
    candidates: &DeploymentSet,
    selected: &BTreeSet<(String, String)>,
) -> DeploymentSet {
    let mut composed = base.clone();
    for (host_name, plane_name) in selected {
        let plane = candidates.hosts[host_name].planes[plane_name].clone();
        composed
            .hosts
            .entry(host_name.clone())
            .or_insert_with(|| ReleaseHostEntry {
                planes: BTreeMap::new(),
            })
            .planes
            .insert(plane_name.clone(), plane);
    }
    composed
}

fn selection_already_present(
    current: Option<&DeploymentSet>,
    candidate: &DeploymentSet,
    selected: &BTreeSet<(String, String)>,
    full: bool,
) -> bool {
    let Some(current) = current else { return false };
    if full {
        return current == candidate;
    }
    selected.iter().all(|(host, plane)| {
        current.hosts.get(host).and_then(|h| h.planes.get(plane))
            == candidate.hosts.get(host).and_then(|h| h.planes.get(plane))
    })
}

fn write_immutable(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PromotionError> {
    let file_name = path
        .file_name()
        .expect("immutable path names a file")
        .to_string_lossy();
    let tmp = path
        .parent()
        .expect("immutable file has parent")
        .join(format!(
            ".{}.{}.immutable.tmp",
            file_name,
            std::process::id()
        ));
    let write = || -> Result<(), PromotionError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| PromotionError::Io(tmp.clone(), e.to_string()))?;
        file.write_all(bytes)
            .map_err(|e| PromotionError::Io(tmp.clone(), e.to_string()))?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| PromotionError::Io(tmp.clone(), e.to_string()))?;
        file.sync_all()
            .map_err(|e| PromotionError::Io(tmp.clone(), e.to_string()))?;
        // A plain rename may replace an existing file. Linking the fully-synced temp into
        // place is atomic and fails with AlreadyExists, preserving the write-once contract
        // even if another process outside the flock misbehaves.
        fs::hard_link(&tmp, path)
            .map_err(|e| PromotionError::Io(path.to_path_buf(), e.to_string()))?;
        sync_dir(path.parent().expect("immutable file has parent"))?;
        fs::remove_file(&tmp).map_err(|e| PromotionError::Io(tmp.clone(), e.to_string()))?;
        sync_dir(path.parent().expect("immutable file has parent"))
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn sync_dir(path: &Path) -> Result<(), PromotionError> {
    let dir =
        fs::File::open(path).map_err(|e| PromotionError::Io(path.to_path_buf(), e.to_string()))?;
    dir.sync_all()
        .map_err(|e| PromotionError::Io(path.to_path_buf(), e.to_string()))
}

fn read(path: &Path) -> Result<Vec<u8>, PromotionError> {
    fs::read(path).map_err(|e| PromotionError::Io(path.to_path_buf(), e.to_string()))
}

pub fn digest_id(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn looks_like_id(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .map(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or(false)
}

fn looks_like_git_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn looks_like_nar_hash(value: &str) -> bool {
    let Some(encoded) = value.strip_prefix("sha256-") else {
        return false;
    };
    BASE64
        .decode(encoded)
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
}

fn looks_like_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 20 {
        return false;
    }
    [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
        .iter()
        .all(|&i| b[i].is_ascii_digit())
        && [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'Z'),
        ]
        .iter()
        .all(|&(i, expected)| b[i] == expected)
}

fn parse_version(value: &str) -> Option<Vec<u64>> {
    let mut parsed = Vec::new();
    for component in value.split('.') {
        let digits: String = component
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return None;
        }
        parsed.push(digits.parse().ok()?);
    }
    (parsed.len() >= 2).then_some(parsed)
}

fn version_less_than(actual: &[u64], required: &[u64]) -> bool {
    let length = actual.len().max(required.len());
    (0..length)
        .map(|index| {
            (
                actual.get(index).copied().unwrap_or(0),
                required.get(index).copied().unwrap_or(0),
            )
        })
        .find(|(a, r)| a != r)
        .map(|(a, r)| a < r)
        .unwrap_or(false)
}

fn display_optional_id(id: Option<&str>) -> &str {
    id.unwrap_or("<none>")
}
