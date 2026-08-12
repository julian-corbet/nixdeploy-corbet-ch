use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::SigningKey;

use nixdeploy::manifest::Backend;
use nixdeploy::manifest::Fetcher;
use nixdeploy::promote::{promote, PromoteArgs};
use nixdeploy::release::{
    verify_release, Artifact, ArtifactProvenance, ArtifactRequirements, BuilderProvenance,
    DeploymentSet, PromotionRequest, PromotionStatus, ReleaseBootSpec, ReleaseHostEntry,
    ReleasePlaneEntry, ReleaseStore, SourceProvenance,
};

const PATH_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system-a";
const PATH_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-system-b";
const PATH_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-system-c";
const WHEN_1: &str = "2026-08-12T06:00:00Z";
const WHEN_2: &str = "2026-08-12T07:00:00Z";

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nixdeploy-release-{}-{}-{}",
        tag,
        std::process::id(),
        serial
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create test directory");
    path
}

fn keys() -> (String, SigningKey) {
    let key = SigningKey::from_bytes(&[47u8; 32]);
    let public = format!(
        "release-1:{}",
        BASE64.encode(key.verifying_key().to_bytes())
    );
    (public, key)
}

fn artifact(path: &str, byte: u8, revision_digit: char) -> Artifact {
    Artifact {
        target: path.to_string(),
        nar_hash: format!("sha256-{}", BASE64.encode([byte; 32])),
        provenance: ArtifactProvenance {
            source: SourceProvenance {
                repository: "https://github.com/example/infra".to_string(),
                revision: std::iter::repeat(revision_digit).take(40).collect(),
                lock_digest: format!("sha256:{}", format!("{:02x}", byte).repeat(32)),
            },
            builder: BuilderProvenance {
                id: "corbet-builder/x86_64-linux".to_string(),
                nix_version: "nix (Determinate Nix 3.22.0) 2.35.1".to_string(),
                store_version: "2.35.1".to_string(),
            },
        },
        requirements: ArtifactRequirements {
            system: "x86_64-linux".to_string(),
            minimum_store_version: "2.18.0".to_string(),
        },
    }
}

fn host(path: &str, byte: u8, revision_digit: char) -> ReleaseHostEntry {
    ReleaseHostEntry {
        planes: [(
            "nixos".to_string(),
            ReleasePlaneEntry {
                backend: Backend::Nixos,
                identity: None,
                artifact: artifact(path, byte, revision_digit),
                boot: Some(ReleaseBootSpec::None),
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn two_hosts() -> BTreeMap<String, ReleaseHostEntry> {
    [
        ("host-a".to_string(), host(PATH_A, 1, 'a')),
        ("host-b".to_string(), host(PATH_B, 2, 'b')),
    ]
    .into_iter()
    .collect()
}

fn request(
    candidates: BTreeMap<String, ReleaseHostEntry>,
    expected_base: Option<String>,
    when: &str,
) -> PromotionRequest {
    PromotionRequest {
        candidates,
        hosts: BTreeSet::new(),
        planes: BTreeSet::new(),
        expected_base,
        published_at: when.to_string(),
    }
}

#[test]
fn deployment_set_identity_is_composition_not_timestamp_or_map_insertion_order() {
    let ordered = DeploymentSet::new(two_hosts());
    let mut reverse = BTreeMap::new();
    reverse.insert("host-b".to_string(), host(PATH_B, 2, 'b'));
    reverse.insert("host-a".to_string(), host(PATH_A, 1, 'a'));
    let reversed = DeploymentSet::new(reverse);

    assert_eq!(ordered.id(), reversed.id());

    let (_, key) = keys();
    let first = nixdeploy::release::sign_release(
        ordered.clone(),
        None,
        WHEN_1.to_string(),
        "release-1",
        &key,
    )
    .expect("sign first");
    let second =
        nixdeploy::release::sign_release(ordered, None, WHEN_2.to_string(), "release-1", &key)
            .expect("sign second");
    let public = format!(
        "release-1:{}",
        BASE64.encode(key.verifying_key().to_bytes())
    );
    let first_doc = verify_release(&first.canonical_bytes(), &public).expect("verify first");
    let second_doc = verify_release(&second.canonical_bytes(), &public).expect("verify second");
    assert_eq!(first_doc.deployment_set_id, second_doc.deployment_set_id);
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
}

#[test]
fn provenance_is_part_of_identity_even_when_the_store_path_is_unchanged() {
    let first = DeploymentSet::new(two_hosts());
    let mut changed = two_hosts();
    changed
        .get_mut("host-a")
        .unwrap()
        .planes
        .get_mut("nixos")
        .unwrap()
        .artifact
        .provenance
        .builder
        .store_version = "2.36.0".to_string();
    let second = DeploymentSet::new(changed);

    assert_ne!(first.id(), second.id());
}

#[test]
fn partial_promotion_preserves_every_unselected_artifact_and_its_provenance() {
    let dir = tmpdir("partial");
    let (public, key) = keys();
    let store = ReleaseStore::new(&dir);

    let initial = store
        .promote(&request(two_hosts(), None, WHEN_1), "release-1", &key)
        .expect("initial promotion");
    assert_eq!(initial.status, PromotionStatus::Promoted);
    let initial_id = initial.deployment_set_id.clone().unwrap();
    let initial_bytes = fs::read(dir.join("channels/stable.json")).expect("read initial");
    let initial_doc = verify_release(&initial_bytes, &public).expect("verify initial");
    let preserved = initial_doc.deployment_set.hosts["host-b"].clone();

    let mut partial = request(
        [("host-a".to_string(), host(PATH_C, 3, 'c'))]
            .into_iter()
            .collect(),
        Some(initial_id.clone()),
        WHEN_2,
    );
    partial.hosts.insert("host-a".to_string());
    let promoted = store
        .promote(&partial, "release-1", &key)
        .expect("partial promotion");
    assert_eq!(promoted.status, PromotionStatus::Promoted);

    let stable = fs::read(dir.join("channels/stable.json")).expect("read stable");
    let doc = verify_release(&stable, &public).expect("verify stable");
    assert_eq!(
        doc.parent_deployment_set_id.as_deref(),
        Some(initial_id.as_str())
    );
    assert_eq!(doc.deployment_set.hosts["host-b"], preserved);
    assert_eq!(
        doc.deployment_set.hosts["host-a"].planes["nixos"]
            .artifact
            .target,
        PATH_C
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn one_atomic_envelope_verifies_exact_payload_bytes_and_detects_mutation() {
    let (public, key) = keys();
    let envelope = nixdeploy::release::sign_release(
        DeploymentSet::new(two_hosts()),
        None,
        WHEN_1.to_string(),
        "release-1",
        &key,
    )
    .expect("sign");
    let bytes = envelope.canonical_bytes();
    verify_release(&bytes, &public).expect("valid envelope");

    let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse envelope");
    let payload = value["payload"].as_str().unwrap().to_string();
    let replacement = if payload.starts_with('A') { 'B' } else { 'A' };
    value["payload"] = format!("{}{}", replacement, &payload[1..]).into();
    let tampered = serde_json::to_vec(&value).unwrap();
    assert!(verify_release(&tampered, &public).is_err());
}

#[test]
fn promotion_is_cas_idempotent_and_stale_completion_is_terminally_superseded() {
    let dir = tmpdir("cas");
    let (_public, key) = keys();
    let store = ReleaseStore::new(&dir);
    let first_request = request(two_hosts(), None, WHEN_1);

    let first = store
        .promote(&first_request, "release-1", &key)
        .expect("first promotion");
    assert_eq!(first.status, PromotionStatus::Promoted);
    assert_eq!(first.generation, Some(1));

    let retry = store
        .promote(&first_request, "release-1", &key)
        .expect("idempotent retry");
    assert_eq!(retry.status, PromotionStatus::Unchanged);
    assert_eq!(retry.generation, Some(1));

    let stale = store
        .promote(
            &request(
                [("host-a".to_string(), host(PATH_C, 3, 'c'))]
                    .into_iter()
                    .collect(),
                None,
                WHEN_2,
            ),
            "release-1",
            &key,
        )
        .expect("stale completion has a terminal outcome");
    assert_eq!(stale.status, PromotionStatus::Superseded);
    assert_eq!(
        fs::read_dir(dir.join("promotions")).unwrap().count(),
        1,
        "superseded work must not poison or advance the promotion journal"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn recovery_restores_exact_channel_bytes_from_signed_immutable_journal() {
    let dir = tmpdir("recovery");
    let (public, key) = keys();
    let store = ReleaseStore::new(&dir);
    let first = store
        .promote(&request(two_hosts(), None, WHEN_1), "release-1", &key)
        .expect("promotion");
    let wanted = fs::read(dir.join("channels/stable.json")).expect("read stable");

    fs::write(dir.join("channels/stable.json"), b"interrupted publish").expect("damage channel");
    assert_eq!(store.recover(&key).unwrap(), first.deployment_set_id);
    let restored = fs::read(dir.join("channels/stable.json")).expect("read restored");
    assert_eq!(restored, wanted);
    verify_release(&restored, &public).expect("restored channel remains trusted");

    fs::remove_dir_all(dir).ok();
}

#[test]
fn compatibility_is_based_on_system_and_store_daemon_not_determinate_branding() {
    let requirements = ArtifactRequirements {
        system: "x86_64-linux".to_string(),
        minimum_store_version: "2.35.0".to_string(),
    };

    requirements
        .check("x86_64-linux", "2.35.1")
        .expect("new enough upstream or Determinate daemon is compatible");
    assert!(requirements.check("aarch64-linux", "2.35.1").is_err());
    assert!(requirements.check("x86_64-linux", "2.34.9").is_err());
}

#[test]
fn invalid_candidate_is_terminally_rejected_without_a_release_or_journal() {
    let dir = tmpdir("rejected");
    let (_public, key) = keys();
    let store = ReleaseStore::new(&dir);
    let mut invalid = two_hosts();
    invalid
        .get_mut("host-a")
        .unwrap()
        .planes
        .get_mut("nixos")
        .unwrap()
        .artifact
        .provenance
        .source
        .revision = "moving-main".to_string();

    let outcome = store
        .promote(&request(invalid, None, WHEN_1), "release-1", &key)
        .expect("invalid work has a terminal outcome");
    assert_eq!(outcome.status, PromotionStatus::Rejected);
    assert!(!outcome.details.is_empty());
    assert_eq!(fs::read_dir(dir.join("releases")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(dir.join("promotions")).unwrap().count(), 0);

    fs::remove_dir_all(dir).ok();
}

#[test]
fn trusted_promote_boundary_writes_a_terminal_result_and_signed_channel() {
    let dir = tmpdir("promote-command");
    let (public, key) = keys();
    let targets = dir.join("targets.json");
    fs::write(&targets, serde_json::to_vec(&two_hosts()).unwrap()).expect("write targets");
    let mut secret = Vec::with_capacity(64);
    secret.extend_from_slice(&key.to_bytes());
    secret.extend_from_slice(&key.verifying_key().to_bytes());
    let key_file = dir.join("release.key");
    fs::write(&key_file, format!("release-1:{}", BASE64.encode(secret))).expect("write key");
    fs::set_permissions(&key_file, fs::Permissions::from_mode(0o600)).expect("chmod key");
    let result_file = dir.join("terminal/request-17.json");
    fs::create_dir_all(result_file.parent().unwrap()).expect("create result directory");

    let result = promote(
        &PromoteArgs {
            targets_file: targets,
            origin: dir.join("origin"),
            expected_base: None,
            hosts: BTreeSet::new(),
            planes: BTreeSet::new(),
            signing_key_file: key_file,
            published_at: Some(WHEN_1.to_string()),
            request_id: "request-17".to_string(),
            result_file: result_file.clone(),
        },
        0,
    )
    .expect("promote command");

    assert_eq!(result.outcome.status, PromotionStatus::Promoted);
    let terminal: serde_json::Value =
        serde_json::from_slice(&fs::read(&result_file).expect("read terminal result"))
            .expect("parse terminal result");
    assert_eq!(terminal["requestId"], "request-17");
    assert_eq!(terminal["outcome"]["status"], "promoted");
    let stable = fs::read(dir.join("origin/channels/stable.json")).expect("read channel");
    verify_release(&stable, &public).expect("channel verifies");

    fs::remove_dir_all(dir).ok();
}

struct OneFileFetcher {
    body: String,
    urls: RefCell<Vec<String>>,
}

impl Fetcher for OneFileFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        self.urls.borrow_mut().push(url.to_string());
        Ok(self.body.clone())
    }
}

#[test]
fn receiver_reads_v4_as_one_file_and_selects_the_exact_artifact() {
    let (public, key) = keys();
    let envelope = nixdeploy::release::sign_release(
        DeploymentSet::new(two_hosts()),
        None,
        WHEN_1.to_string(),
        "release-1",
        &key,
    )
    .expect("sign release");
    let fetcher = OneFileFetcher {
        body: String::from_utf8(envelope.canonical_bytes()).unwrap(),
        urls: RefCell::new(Vec::new()),
    };

    let target = nixdeploy::manifest::fetch_and_verify(
        &fetcher,
        "https://releases.example/channels/stable.json",
        &public,
        "host-a",
        "nixos",
    )
    .expect("select v4 target");
    assert_eq!(target.store_path, PATH_A);
    assert!(target.deployment_set_id.is_some());
    assert_eq!(
        target.artifact.unwrap().provenance.source.revision,
        "a".repeat(40)
    );
    assert_eq!(
        fetcher.urls.into_inner(),
        vec!["https://releases.example/channels/stable.json"],
        "v4 must never fetch a detached .sig sibling"
    );
}
