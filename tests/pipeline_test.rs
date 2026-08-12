//! The assembly, not the parts: publish a real signed manifest, then run a real receiver
//! against those exact bytes, all the way through delta sizing to an activation that actually
//! changes something observable.
//!
//! Every stage of this pipeline already had unit tests against its own fakes, and the wiring
//! between them had none -- which is the shape of bug a green suite survives, because each
//! part works and the assembly is what is wrong. These tests exist to make the seams fail:
//! a signature that covers different bytes than the verifier checks, a delta computed against
//! the wrong path, an activation that runs when the receiver should have refused, a refusal
//! that is never routed anywhere, a metrics write that happens too late to survive the call
//! that kills the process.
//!
//! Nothing here mocks the receiver's own logic. The `Env` implementation below supplies only
//! what is genuinely outside this process -- a hostname, manifest bytes, a store's contents,
//! a substituter's narinfo answers and a clock. Signature verification, closure walking,
//! ceiling arithmetic, command execution and outcome construction are all the real code.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::SigningKey;

use nixdeploy::delta::{Delta, DeltaError, LocalStore, Narinfo, NarinfoSource};
use nixdeploy::manifest::{BootRole, Fetcher};
use nixdeploy::publish::{publish, PublishArgs};
use nixdeploy::receive::{CompatibilityFacts, DeltaSources, Env, ReceiverConfig};
use nixdeploy::{Outcome, RefusedReason, Stage};

const OLD_PATH: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system-old";
const NEW_PATH: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-system-new";
const DEP_PATH: &str = "/nix/store/cccccccccccccccccccccccccccccccc-dependency";
const BOOT_ARTIFACT: &str = "/nix/store/dddddddddddddddddddddddddddddddd-primary-boot";
const RESCUE_ARTIFACT: &str = "/nix/store/ffffffffffffffffffffffffffffffff-nixrescue-boot";
const RESCUE_IMAGE: &str = "image-host-a-nixrescue-2026-08";
const MANIFEST_URL: &str = "https://example.org/nixdeploy/manifest.json";
const IMAGE: &str = "image-host-a-2026-08";

// ---------------------------------------------------------------------------------------
// The world outside the process
// ---------------------------------------------------------------------------------------

/// Serves the bytes `publish` actually wrote, from disk, the way an HTTP origin would.
struct FileFetcher {
    manifest: PathBuf,
    /// Applied to the manifest body after reading it -- how a tampering test puts different
    /// bytes in front of the receiver than the ones the publisher signed.
    tamper: Option<fn(String) -> String>,
}

impl Fetcher for FileFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        let path = if let Some(base) = url.strip_suffix(".sig") {
            assert_eq!(base, MANIFEST_URL, "unexpected signature URL {}", url);
            let mut p = self.manifest.clone().into_os_string();
            p.push(".sig");
            PathBuf::from(p)
        } else {
            assert_eq!(url, MANIFEST_URL, "unexpected manifest URL {}", url);
            self.manifest.clone()
        };

        let body = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if url.ends_with(".sig") {
            return Ok(body);
        }
        Ok(match self.tamper {
            Some(f) => f(body),
            None => body,
        })
    }
}

struct FakeStore {
    present: HashSet<String>,
    unanswerable: Option<String>,
}

impl LocalStore for FakeStore {
    fn is_present(&self, store_path: &str) -> Result<bool, DeltaError> {
        match &self.unanswerable {
            Some(detail) => Err(DeltaError::LocalStoreQuery(
                store_path.to_string(),
                detail.clone(),
            )),
            None => Ok(self.present.contains(store_path)),
        }
    }
}

struct FakeNarinfo {
    info: HashMap<String, Narinfo>,
}

impl NarinfoSource for FakeNarinfo {
    fn fetch(&self, store_path: &str) -> Result<Narinfo, DeltaError> {
        self.info.get(store_path).cloned().ok_or_else(|| {
            DeltaError::NarinfoFetch(store_path.to_string(), "no such path in fake cache".into())
        })
    }
}

struct TestEnv {
    hostname: String,
    fetcher: FileFetcher,
    /// Paths this machine's store already holds.
    present: HashSet<String>,
    /// What the substituter says everything else costs.
    sizes: HashMap<String, Narinfo>,
    /// Every path `delta_sources` was asked to build sources for, so a test can prove the
    /// delta stage was reached (or was not).
    delta_calls: RefCell<usize>,
    store_error: Option<String>,
}

impl Env for TestEnv {
    fn hostname(&self) -> Result<String, String> {
        Ok(self.hostname.clone())
    }

    fn fetcher(&self) -> &dyn Fetcher {
        &self.fetcher
    }

    fn delta_sources(&self, _cfg: &ReceiverConfig) -> Result<DeltaSources, String> {
        *self.delta_calls.borrow_mut() += 1;
        Ok(DeltaSources {
            store: Box::new(FakeStore {
                present: self.present.clone(),
                unanswerable: self.store_error.clone(),
            }),
            narinfo: Box::new(FakeNarinfo {
                info: self.sizes.clone(),
            }),
        })
    }

    fn compatibility_facts(&self, _cfg: &ReceiverConfig) -> Result<CompatibilityFacts, String> {
        Ok(CompatibilityFacts {
            system: "x86_64-linux".to_string(),
            store_version: "2.35.1".to_string(),
        })
    }

    fn now_unix(&self) -> u64 {
        1_785_758_400
    }
}

// ---------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    public_key: String,
    /// The file the fake activation adapter reads and writes. Holding the "currently running"
    /// store path in a file is what makes activation OBSERVABLE: a receiver that refuses must
    /// leave it untouched, and one that converges must have changed it.
    state: PathBuf,
    manifest: PathBuf,
}

impl Fixture {
    /// Publishes a real manifest for `host-a` and returns everything a receiver needs to be
    /// pointed at it.
    fn new(tag: &str, image: Option<&str>) -> Fixture {
        Self::new_for_plane(tag, "nixos", None, image)
    }

    fn new_home_manager(tag: &str, identity: &str) -> Fixture {
        Self::new_for_plane(tag, "home-manager", Some(identity), None)
    }

    fn new_for_plane(
        tag: &str,
        plane: &str,
        identity: Option<&str>,
        image: Option<&str>,
    ) -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("nixdeploy-pipeline-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tmpdir");

        let seed = [23u8; 32];
        let signing = SigningKey::from_bytes(&seed);
        let mut secret = Vec::with_capacity(64);
        secret.extend_from_slice(&seed);
        secret.extend_from_slice(&signing.verifying_key().to_bytes());
        let key_file = dir.join("signing.key");
        fs::write(&key_file, format!("cache-1:{}", BASE64.encode(secret))).expect("write key");
        fs::set_permissions(&key_file, fs::Permissions::from_mode(0o600)).expect("chmod key");

        let targets_file = dir.join("targets.json");
        let boot_json = if plane == "nixos" {
            let mut primary = serde_json::json!({ "artifact": BOOT_ARTIFACT });
            if let Some(image) = image {
                primary
                    .as_object_mut()
                    .unwrap()
                    .insert("image".to_string(), image.into());
            }
            format!(
                ",\"boot\":{}",
                serde_json::json!({
                    "mode": "managed",
                    "roles": {
                        "primary": primary,
                        "nixrescue": {
                            "artifact": RESCUE_ARTIFACT,
                            "image": RESCUE_IMAGE
                        }
                    }
                })
            )
        } else {
            String::new()
        };
        let identity_json = match identity {
            Some(i) => format!(",\"identity\":\"{}\"", i),
            None => String::new(),
        };
        fs::write(
            &targets_file,
            format!(
                r#"{{"host-a":{{"planes":{{"{plane}":{{"backend":"{plane}","target":"{target}"{identity}{boot}}}}}}}}}"#,
                plane = plane,
                target = NEW_PATH,
                identity = identity_json,
                boot = boot_json,
            ),
        )
        .expect("write targets");

        let manifest = dir.join("manifest.json");
        publish(
            &PublishArgs {
                targets_file,
                base_manifest: None,
                hosts: Default::default(),
                planes: Default::default(),
                revision: "rev-1".to_string(),
                built_at: Some("2026-08-03T12:00:00Z".to_string()),
                signing_key_file: key_file,
                out: manifest.clone(),
            },
            0,
        )
        .expect("publish");

        let state = dir.join("current-path");
        fs::write(&state, OLD_PATH).expect("seed state");

        Fixture {
            dir,
            public_key: format!(
                "cache-1:{}",
                BASE64.encode(signing.verifying_key().to_bytes())
            ),
            state,
            manifest,
        }
    }

    fn textfile(&self) -> PathBuf {
        self.dir.join("nixdeploy.prom")
    }

    /// A receiver config in exactly the JSON shape a Nix module renders. Written to disk and
    /// loaded back through the real `load_config`, so a field this crate cannot actually
    /// parse fails here rather than in production.
    fn config(&self, ceiling: Option<u64>, reimage: Option<&str>, metrics: bool) -> PathBuf {
        self.config_for_plane("nixos", None, ceiling, reimage, metrics)
    }

    fn boot_reconcile_config(&self, command: &str) -> PathBuf {
        let path = self.config(Some(10_000), None, true);
        let mut document: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).expect("read receiver config for boot reconciler"),
        )
        .expect("parse receiver config for boot reconciler");
        document.as_object_mut().expect("config object").insert(
            "bootRoleReconcile".to_string(),
            serde_json::json!({ "command": command, "role": "nixrescue" }),
        );
        fs::write(
            &path,
            serde_json::to_vec_pretty(&document).expect("serialize boot reconciler config"),
        )
        .expect("write boot reconciler config");
        path
    }

    fn home_manager_config(&self, identity: &str) -> PathBuf {
        self.config_for_plane("home-manager", Some(identity), Some(10_000), None, false)
    }

    fn config_for_plane(
        &self,
        plane: &str,
        identity: Option<&str>,
        ceiling: Option<u64>,
        reimage: Option<&str>,
        metrics: bool,
    ) -> PathBuf {
        let receiver_state = self.dir.join("receiver-state");
        fs::create_dir_all(&receiver_state).expect("create receiver state directory");
        let activate = sh(&format!("printf %s $0 > {}", self.state.display()));
        let current = sh(&format!("cat {}", self.state.display()));
        let metrics_json = if metrics {
            format!(r#"{{"textfile":"{}"}}"#, self.textfile().display())
        } else {
            "{}".to_string()
        };
        let identity_json = identity
            .map(|i| format!(", \"identity\": \"{}\"", i))
            .unwrap_or_default();

        let json = format!(
            r#"{{
                "manifest": {{ "url": "{url}", "publicKey": "{key}" }},
                "plane": {{ "name": "{plane}", "backend": "{plane}"{identity} }},
                "stateDirectory": "{state_directory}",
                {ceiling}
                "activation": {{ "activate": "{activate}", "currentPath": "{current}" }},
                "healthGate": [],
                {reimage}
                "metrics": {metrics_json}
            }}"#,
            url = MANIFEST_URL,
            key = self.public_key,
            plane = plane,
            identity = identity_json,
            state_directory = receiver_state.display(),
            ceiling = ceiling
                .map(|c| format!("\"maxInplaceDeltaBytes\": {},", c))
                .unwrap_or_default(),
            activate = activate,
            current = current,
            reimage = reimage
                .map(|c| format!(
                    "\"reimage\": {{ \"command\": {}, \"role\": \"primary\" }},",
                    serde_json::to_string(c).expect("serialize reimage command")
                ))
                .unwrap_or_default(),
            metrics_json = metrics_json,
        );

        let path = self.dir.join("config.json");
        fs::write(&path, json).expect("write config");
        path
    }

    /// A receiver whose candidate really activates, fails its health gate, and can roll
    /// back to the previously observed closure. The attempt file makes a repeat activation
    /// observable independently of the current-path file that rollback restores.
    fn poison_config(&self) -> (PathBuf, PathBuf, PathBuf) {
        let receiver_state = self.dir.join("poison-state");
        fs::create_dir_all(&receiver_state).expect("create receiver state directory");
        let attempts = self.dir.join("activation-attempts");
        let activate = sh(&format!(
            "printf %s $0 > {state}; printf x >> {attempts}",
            state = self.state.display(),
            attempts = attempts.display(),
        ));
        let current = sh(&format!("cat {}", self.state.display()));
        let rollback = sh(&format!(
            "printf %s {old} > {state}",
            old = OLD_PATH,
            state = self.state.display()
        ));

        let json = format!(
            r#"{{
                "manifest": {{ "url": "{url}", "publicKey": "{key}" }},
                "plane": {{ "name": "nixos", "backend": "nixos" }},
                "stateDirectory": "{state_directory}",
                "maxInplaceDeltaBytes": 10000,
                "activation": {{
                    "activate": "{activate}",
                    "currentPath": "{current}",
                    "rollback": "{rollback}"
                }},
                "healthGate": ["{health_gate}"],
                "metrics": {{}}
            }}"#,
            url = MANIFEST_URL,
            key = self.public_key,
            state_directory = receiver_state.display(),
            activate = activate,
            current = current,
            rollback = rollback,
            health_gate = sh("exit 1"),
        );
        let config = self.dir.join("poison-config.json");
        fs::write(&config, json).expect("write poison config");
        (config, receiver_state, attempts)
    }

    fn env(&self, tamper: Option<fn(String) -> String>) -> TestEnv {
        // The target is missing and pulls in one dependency (200 + 300 bytes); the old
        // system is present, so a walk that ever asked about it would fail the fake cache.
        let mut sizes = HashMap::new();
        sizes.insert(
            NEW_PATH.to_string(),
            Narinfo {
                nar_size: 200,
                references: vec![DEP_PATH.to_string(), OLD_PATH.to_string()],
            },
        );
        sizes.insert(
            DEP_PATH.to_string(),
            Narinfo {
                nar_size: 300,
                references: vec![],
            },
        );

        TestEnv {
            hostname: "host-a".to_string(),
            fetcher: FileFetcher {
                manifest: self.manifest.clone(),
                tamper,
            },
            present: [OLD_PATH.to_string()].into_iter().collect(),
            sizes,
            delta_calls: RefCell::new(0),
            store_error: None,
        }
    }

    fn current_path(&self) -> String {
        fs::read_to_string(&self.state).expect("read state")
    }

    fn metrics_text(&self) -> String {
        fs::read_to_string(self.textfile()).expect("read metrics textfile")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Same reasoning as `activate.rs`'s own test helper: `/bin/sh` is a long-lived binary
/// nothing here writes to, so exec'ing it cannot race a freshly-written script file the way
/// a generated temp executable can (a real, reproducible `ETXTBSY` during this crate's
/// development).
fn sh(body: &str) -> String {
    format!("/bin/sh -c '{}'", body)
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[test]
fn a_published_manifest_drives_a_real_convergence() {
    let fixture = Fixture::new("converge", None);
    let config = fixture.config(Some(10_000), None, true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    assert_eq!(
        outcome,
        Outcome::Converged {
            from: OLD_PATH.to_string(),
            to: NEW_PATH.to_string(),
        },
        "publish -> verify -> size -> activate should end in a confirmed convergence"
    );
    assert_eq!(
        fixture.current_path(),
        NEW_PATH,
        "the activate command must actually have been given the target path"
    );

    // The delta the receiver measured is the one an operator watching this machine sees,
    // and it must be the sum of only the MISSING paths -- 200 + 300, never the present old
    // system too.
    let metrics = fixture.metrics_text();
    assert!(
        metrics.contains("nixdeploy_delta_bytes 500\n"),
        "delta should be the two missing paths only:\n{}",
        metrics
    );
    assert!(metrics.contains("nixdeploy_run_outcome{outcome=\"converged\"} 1\n"));
    assert!(metrics.contains("nixdeploy_run_outcome{outcome=\"refused\"} 0\n"));
    assert!(metrics.contains("nixdeploy_reimage_owed 0\n"));
    assert!(metrics.contains("nixdeploy_delta_ceiling_bytes 10000\n"));
    assert!(metrics.contains("nixdeploy_run_timestamp_seconds 1785758400\n"));
}

#[test]
fn a_second_run_against_the_same_manifest_reports_already_current() {
    let fixture = Fixture::new("already", None);
    let config = fixture.config(Some(10_000), None, true);
    let env = fixture.env(None);

    let first = nixdeploy::receive::run_with(&config, &env);
    assert!(
        matches!(first, Outcome::Converged { .. }),
        "the first run should have converged, got {:?}",
        first
    );
    let second = nixdeploy::receive::run_with(&config, &env);

    assert_eq!(
        second,
        Outcome::AlreadyCurrent {
            rev: NEW_PATH.to_string()
        },
        "a machine already on the target must not report the same thing as one that changed"
    );
    assert_eq!(
        *env.delta_calls.borrow(),
        1,
        "a machine that is already current must not size a delta at all"
    );

    // The delta metric from the first run must not be restated as if it were measured now:
    // an unmeasured delta is absent, not zero and not stale.
    let metrics = fixture.metrics_text();
    assert!(
        !metrics.contains("nixdeploy_delta_bytes"),
        "no delta was measured on this run:\n{}",
        metrics
    );
    assert!(metrics.contains("nixdeploy_run_outcome{outcome=\"alreadyCurrent\"} 1\n"));
}

#[test]
fn a_boot_role_is_reconciled_after_health_and_again_when_already_current() {
    let fixture = Fixture::new("boot-reconcile", None);
    let seen = fixture.dir.join("boot-reconcile-arguments");
    let command = sh(&format!("printf \"%s\\n\" \"$0\" >> {}", seen.display()));
    let config = fixture.boot_reconcile_config(&command);
    let env = fixture.env(None);

    assert!(matches!(
        nixdeploy::receive::run_with(&config, &env),
        Outcome::Converged { .. }
    ));
    assert!(matches!(
        nixdeploy::receive::run_with(&config, &env),
        Outcome::AlreadyCurrent { .. }
    ));
    assert_eq!(
        fs::read_to_string(&seen).expect("boot reconciler should have run"),
        format!("{0}\n{0}\n", RESCUE_ARTIFACT),
        "both the healthy activation and self-correction pass receive the exact signed role"
    );
}

#[test]
fn a_failed_boot_reconcile_is_loud_without_rolling_back_a_healthy_system() {
    let fixture = Fixture::new("boot-reconcile-failed", None);
    let config = fixture.boot_reconcile_config(&sh("exit 23"));
    let env = fixture.env(None);

    match nixdeploy::receive::run_with(&config, &env) {
        Outcome::Failed { stage, detail } => {
            assert_eq!(stage, Stage::BootReconcile);
            assert!(
                detail.contains("nixrescue boot-role reconciler"),
                "{detail}"
            );
        }
        other => panic!("want boot reconciliation failure, got {other:?}"),
    }
    assert_eq!(
        fixture.current_path(),
        NEW_PATH,
        "boot durability failure must not roll back a system that passed health checks"
    );
}

#[test]
fn a_health_rejected_immutable_target_is_rolled_back_once_and_never_activated_again() {
    let fixture = Fixture::new("poison-pin", None);
    let (config, receiver_state, attempts) = fixture.poison_config();
    let env = fixture.env(None);

    let first = nixdeploy::receive::run_with(&config, &env);
    match &first {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::HealthCheckFailed);
            assert!(detail.contains("rolled back"), "detail was: {}", detail);
        }
        other => panic!("want the first health rejection, got {:?}", other),
    }
    assert_eq!(
        fixture.current_path(),
        OLD_PATH,
        "the failed target must be rolled back"
    );
    assert_eq!(fs::read_to_string(&attempts).unwrap(), "x");

    let pin = receiver_state.join("rejected-target-nixos.json");
    let pin_json = fs::read_to_string(&pin).expect("health rejection must be persisted");
    assert!(pin_json.contains(NEW_PATH), "pin was: {}", pin_json);
    assert_eq!(
        fs::metadata(&pin).unwrap().permissions().mode() & 0o777,
        0o600,
        "receiver safety state is private service state"
    );

    let second = nixdeploy::receive::run_with(&config, &env);
    match &second {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::RejectedTarget);
            assert!(detail.contains(NEW_PATH), "detail was: {}", detail);
            assert!(
                detail.contains(&pin.display().to_string()),
                "detail was: {}",
                detail
            );
        }
        other => panic!("want a typed rejected-target outcome, got {:?}", other),
    }
    assert_eq!(fixture.current_path(), OLD_PATH);
    assert_eq!(
        fs::read_to_string(&attempts).unwrap(),
        "x",
        "the pinned immutable target must not be activated a second time"
    );
    assert_eq!(
        *env.delta_calls.borrow(),
        1,
        "the pin must stop a repeat before even recomputing the same delta"
    );
}

#[test]
fn unreadable_rejection_state_fails_closed_before_delta_or_activation() {
    let fixture = Fixture::new("poison-state-corrupt", None);
    let config = fixture.config(Some(10_000), None, false);
    let pin = fixture
        .dir
        .join("receiver-state")
        .join("rejected-target-nixos.json");
    fs::write(&pin, "not json").expect("seed corrupt pin");
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::State);
            assert!(detail.contains("parsing"), "detail was: {}", detail);
            assert!(
                detail.contains(&pin.display().to_string()),
                "detail was: {}",
                detail
            );
        }
        other => panic!("want a persistent-state failure, got {:?}", other),
    }
    assert_eq!(fixture.current_path(), OLD_PATH);
    assert_eq!(
        *env.delta_calls.borrow(),
        0,
        "unknown poison-pin state must stop before the candidate is touched"
    );
}

#[test]
fn a_different_target_that_converges_clears_the_stale_poison_pin() {
    let fixture = Fixture::new("poison-pin-clear", None);
    let config = fixture.config(Some(10_000), None, false);
    let pin = fixture
        .dir
        .join("receiver-state")
        .join("rejected-target-nixos.json");
    fs::write(
        &pin,
        r#"{
            "version": 1,
            "plane": "nixos",
            "target": "/nix/store/dddddddddddddddddddddddddddddddd-older-bad-target",
            "rejectedAt": 1785758300
        }"#,
    )
    .expect("seed stale poison pin");

    let outcome = nixdeploy::receive::run_with(&config, &fixture.env(None));

    assert_eq!(
        outcome,
        Outcome::Converged {
            from: OLD_PATH.to_string(),
            to: NEW_PATH.to_string(),
        }
    );
    assert!(
        !pin.exists(),
        "a different target that passes health supersedes the old poison pin"
    );
}

#[test]
fn over_the_ceiling_with_no_reimage_command_refuses_and_stops() {
    let fixture = Fixture::new("refuse", Some(IMAGE));
    let config = fixture.config(Some(499), None, true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    assert_eq!(
        outcome,
        Outcome::Refused {
            reason: RefusedReason::DeltaExceedsCeiling,
            bytes: 500,
            ceiling: 499,
        },
        "refusing with the numbers is the whole point; it must stay possible with no \
         reimage configured"
    );
    assert!(!outcome.is_error(), "refusing is not a failure");
    assert_eq!(
        fixture.current_path(),
        OLD_PATH,
        "a refusal must never have activated anything"
    );

    let metrics = fixture.metrics_text();
    assert!(
        metrics.contains("nixdeploy_reimage_owed 1\n"),
        "{}",
        metrics
    );
    assert!(
        metrics.contains("nixdeploy_delta_bytes 500\n"),
        "{}",
        metrics
    );
    assert!(
        metrics.contains("nixdeploy_delta_ceiling_bytes 499\n"),
        "{}",
        metrics
    );
}

#[test]
fn over_the_ceiling_with_a_reimage_command_records_the_refusal_before_invoking_it() {
    let fixture = Fixture::new("reimage", Some(IMAGE));
    // The reimage command copies the metrics textfile aside and records the signed
    // role/artifact/image tuple.
    // Copying it PROVES the refusal was written before the command ran -- which is the
    // whole contract, because on a real provider this call can kill the process that made
    // it, and whatever was written before it is the last thing the machine ever says.
    let seen = fixture.dir.join("reimage-argument");
    let snapshot = fixture.dir.join("metrics-at-reimage-time");
    let command = sh(&format!(
        "printf \"%s\\n%s\\n%s\" \"$0\" \"$1\" \"$2\" > {seen} && cp {textfile} {snapshot}",
        seen = seen.display(),
        textfile = fixture.textfile().display(),
        snapshot = snapshot.display(),
    ));
    let config = fixture.config(Some(499), Some(&command), true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    assert_eq!(
        outcome,
        Outcome::Reimaged {
            role: BootRole::Primary,
            artifact: BOOT_ARTIFACT.to_string(),
            image: IMAGE.to_string()
        },
        "a refusal with a reimage command configured must route to a reimage"
    );
    assert!(!outcome.is_error(), "being replaced is not a failure");
    assert_eq!(
        fixture.current_path(),
        OLD_PATH,
        "reimaging must never have activated in place as well"
    );
    assert_eq!(
        fs::read_to_string(&seen).expect("reimage command should have run"),
        format!("primary\n{}\n{}", BOOT_ARTIFACT, IMAGE),
        "the reimage command must receive the signed role, artifact, and image"
    );

    let at_reimage_time =
        fs::read_to_string(&snapshot).expect("metrics must exist BEFORE the reimage runs");
    assert!(
        at_reimage_time.contains("nixdeploy_run_outcome{outcome=\"refused\"} 1\n"),
        "the refusal must be on the record before the call that may not return:\n{}",
        at_reimage_time
    );
    assert!(
        at_reimage_time.contains("nixdeploy_reimage_owed 1\n"),
        "{}",
        at_reimage_time
    );

    // And after a surviving call, the final report says a reimage was asked for -- while
    // still saying one is owed, because nothing here has observed the replacement.
    let after = fixture.metrics_text();
    assert!(
        after.contains("nixdeploy_run_outcome{outcome=\"reimaged\"} 1\n"),
        "{}",
        after
    );
    assert!(
        after.contains("nixdeploy_run_outcome{outcome=\"refused\"} 0\n"),
        "the earlier refusal must be retired, not left standing:\n{}",
        after
    );
    assert!(
        after.contains("nixdeploy_reimage_owed 1\n"),
        "a requested reimage is still owed until some later run converges:\n{}",
        after
    );
}

#[test]
fn a_reimage_command_the_provider_rejects_is_a_loud_failure() {
    let fixture = Fixture::new("reimage-rejected", Some(IMAGE));
    let config = fixture.config(Some(499), Some(&sh("exit 1")), true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Reimage);
            assert!(detail.contains("no route left"), "detail was: {}", detail);
        }
        other => panic!("want Failed at the reimage stage, got {:?}", other),
    }
    assert!(
        outcome.is_error(),
        "a machine that can neither activate nor be replaced is not a healthy outcome"
    );
    assert_eq!(fixture.current_path(), OLD_PATH);
}

#[test]
fn a_reimage_command_with_no_image_in_the_manifest_is_a_failure_not_an_invention() {
    // host-a's signed primary role has no provider image, but this machine has a reimage
    // command configured: the operator wired a route that cannot be taken. The receiver must
    // say so rather than calling the command with an empty or guessed image.
    let fixture = Fixture::new("reimage-no-image", None);
    let ran = fixture.dir.join("reimage-ran");
    let command = sh(&format!("printf ran > {}", ran.display()));
    let config = fixture.config(Some(499), Some(&command), true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Reimage);
            assert!(
                detail.contains("names no provider image"),
                "detail was: {}",
                detail
            );
        }
        other => panic!("want Failed at the reimage stage, got {:?}", other),
    }
    assert!(
        !ran.exists(),
        "the reimage command must not be invoked with no image to name"
    );
}

#[test]
fn nixrescue_is_signed_but_the_unimplemented_actuator_refuses_without_running() {
    let fixture = Fixture::new("nixrescue-refusal", Some(IMAGE));
    let ran = fixture.dir.join("nixrescue-reimage-ran");
    let command = sh(&format!("printf ran > {}", ran.display()));
    let config = fixture.config(Some(499), Some(&command), true);
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace(r#""role": "primary""#, r#""role": "nixrescue""#),
    )
    .unwrap();

    let outcome = nixdeploy::receive::run_with(&config, &fixture.env(None));

    match outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(stage, Stage::Reimage);
            assert!(detail.contains("supports only role primary"), "{detail}");
            assert!(detail.contains("nixrescue"), "{detail}");
        }
        other => panic!("want a typed nixrescue actuator refusal, got {other:?}"),
    }
    assert!(
        !ran.exists(),
        "an unsupported actuator must never be invoked"
    );
}

#[test]
fn reimage_debt_is_private_durable_state_and_survives_a_later_failed_run() {
    let fixture = Fixture::new("sticky-reimage-debt", Some(IMAGE));
    let config = fixture.config(Some(499), None, true);

    let refused = nixdeploy::receive::run_with(&config, &fixture.env(None));
    assert!(matches!(refused, Outcome::Refused { .. }));

    let marker = fixture
        .dir
        .join("receiver-state")
        .join("reimage-owed-nixos.json");
    assert!(
        marker.exists(),
        "over-ceiling refusal must persist its debt"
    );
    assert_eq!(
        fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let state = fs::read_to_string(&marker).unwrap();
    assert!(state.contains("\"role\": \"primary\""), "{state}");
    assert!(state.contains(BOOT_ARTIFACT), "{state}");

    let failed =
        nixdeploy::receive::run_with(&config, &fixture.env(Some(|body| format!("{} ", body))));
    assert!(matches!(
        failed,
        Outcome::Failed {
            stage: Stage::Manifest,
            ..
        }
    ));
    assert!(
        fixture
            .metrics_text()
            .contains("nixdeploy_reimage_owed 1\n"),
        "debt must stay alertable across unrelated failed ticks"
    );
}

#[test]
fn a_later_observed_convergence_clears_reimage_debt() {
    let fixture = Fixture::new("clear-reimage-debt", Some(IMAGE));
    let refused_config = fixture.config(Some(499), None, true);
    assert!(matches!(
        nixdeploy::receive::run_with(&refused_config, &fixture.env(None)),
        Outcome::Refused { .. }
    ));

    let marker = fixture
        .dir
        .join("receiver-state")
        .join("reimage-owed-nixos.json");
    assert!(marker.exists());

    let converge_config = fixture.config(Some(10_000), None, true);
    let outcome = nixdeploy::receive::run_with(&converge_config, &fixture.env(None));
    assert!(matches!(outcome, Outcome::Converged { .. }));
    assert!(!marker.exists());
    assert!(fixture
        .metrics_text()
        .contains("nixdeploy_reimage_owed 0\n"));
}

#[test]
fn an_unanswerable_store_query_never_reaches_reimage() {
    let fixture = Fixture::new("store-unanswerable", Some(IMAGE));
    let ran = fixture.dir.join("unsafe-reimage-ran");
    let command = sh(&format!("printf ran > {}", ran.display()));
    let config = fixture.config(Some(1), Some(&command), true);
    let mut env = fixture.env(None);
    env.store_error = Some("store database is unavailable".to_string());

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(stage, Stage::Delta);
            assert!(detail.contains("store database is unavailable"), "{detail}");
        }
        other => panic!("want a fail-closed delta error, got {other:?}"),
    }
    assert!(!ran.exists());
    assert!(!fixture
        .dir
        .join("receiver-state")
        .join("reimage-owed-nixos.json")
        .exists());
}

#[test]
fn a_tampered_manifest_never_reaches_the_store_or_the_activation() {
    let fixture = Fixture::new("tampered", None);
    let config = fixture.config(Some(10_000), None, true);
    // Same signature, different bytes underneath it: the store path is swapped for one the
    // publisher never signed. This is the whole reason the manifest is verified before
    // anything in it is trusted.
    let env = fixture.env(Some(|body: String| {
        body.replace(NEW_PATH, "/nix/store/dddddddddddddddddddddddddddddddd-evil")
    }));

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Manifest);
            assert!(detail.contains("signature"), "detail was: {}", detail);
        }
        other => panic!("want a Manifest failure, got {:?}", other),
    }
    assert_eq!(
        *env.delta_calls.borrow(),
        0,
        "nothing may be sized, fetched or activated from a manifest that did not verify"
    );
    assert_eq!(fixture.current_path(), OLD_PATH);
}

#[test]
fn a_home_manager_manifest_activates_only_for_its_signed_identity() {
    let fixture = Fixture::new_home_manager("home-identity", "alice");
    let wrong_config = fixture.home_manager_config("bob");
    let env = fixture.env(None);

    let rejected = nixdeploy::receive::run_with(&wrong_config, &env);

    match &rejected {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Manifest);
            assert!(detail.contains("identity"), "detail was: {}", detail);
            assert!(detail.contains("alice"), "detail was: {}", detail);
            assert!(detail.contains("bob"), "detail was: {}", detail);
        }
        other => panic!("want a signed-identity mismatch, got {:?}", other),
    }
    assert_eq!(
        *env.delta_calls.borrow(),
        0,
        "an identity mismatch must be rejected before sizing or activation"
    );
    assert_eq!(fixture.current_path(), OLD_PATH);

    let matching_config = fixture.home_manager_config("alice");
    let accepted = nixdeploy::receive::run_with(&matching_config, &env);

    assert_eq!(
        accepted,
        Outcome::Converged {
            from: OLD_PATH.to_string(),
            to: NEW_PATH.to_string(),
        },
        "the same signed Home Manager target must converge for its declared owner"
    );
    assert_eq!(fixture.current_path(), NEW_PATH);
}

#[test]
fn a_host_missing_from_the_manifest_is_reported_and_changes_nothing() {
    let fixture = Fixture::new("unknown-host", None);
    let config = fixture.config(Some(10_000), None, true);
    let mut env = fixture.env(None);
    env.hostname = "host-z".to_string();

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Manifest);
            assert!(detail.contains("host-z"), "detail was: {}", detail);
        }
        other => panic!("want a Manifest failure, got {:?}", other),
    }
    assert_eq!(fixture.current_path(), OLD_PATH);

    // A machine the publisher does not know about still reports -- and reports a fresh
    // timestamp -- because "nobody is publishing for me" is exactly the silent failure a
    // staleness alert has to be able to see.
    let metrics = fixture.metrics_text();
    assert!(metrics.contains("nixdeploy_run_timestamp_seconds 1785758400\n"));
    assert!(metrics.contains("nixdeploy_run_outcome{outcome=\"failed\"} 1\n"));
}

#[test]
fn a_receiver_plane_name_that_disagrees_with_its_backend_is_a_config_failure() {
    let fixture = Fixture::new("backend-mismatch", None);
    let config = fixture.config(Some(10_000), None, true);
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace(
            r#""plane": { "name": "nixos", "backend": "nixos" }"#,
            r#""plane": { "name": "nixos", "backend": "system-manager" }"#,
        ),
    )
    .unwrap();
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Config);
            assert!(detail.contains("must equal"), "detail was: {}", detail);
        }
        other => panic!("want a config mismatch, got {:?}", other),
    }
    assert_eq!(*env.delta_calls.borrow(), 0);
    assert_eq!(fixture.current_path(), OLD_PATH);
}

#[test]
fn no_ceiling_means_no_refusal_however_large_the_change() {
    let fixture = Fixture::new("no-ceiling", Some(IMAGE));
    let config = fixture.config(None, Some(&sh("exit 0")), true);
    let env = fixture.env(None);

    let outcome = nixdeploy::receive::run_with(&config, &env);

    assert!(
        matches!(outcome, Outcome::Converged { .. }),
        "a null ceiling is a deliberate 'this machine is large enough', not an untuned \
         placeholder that should refuse: got {:?}",
        outcome
    );
    assert_eq!(fixture.current_path(), NEW_PATH);
    assert!(
        fixture
            .metrics_text()
            .contains("nixdeploy_reimage_owed 0\n"),
        "nothing is owed when nothing refused"
    );
    assert!(
        !fixture
            .metrics_text()
            .contains("nixdeploy_delta_ceiling_bytes"),
        "no ceiling configured is not a ceiling of zero"
    );
}

#[test]
fn broken_metrics_sinks_never_change_the_outcome() {
    let fixture = Fixture::new("broken-sinks", None);
    let state = fixture.state.display().to_string();
    let receiver_state = fixture.dir.join("broken-metrics-state");
    let json = format!(
        r#"{{
            "manifest": {{ "url": "{url}", "publicKey": "{key}" }},
            "plane": {{ "name": "nixos", "backend": "nixos" }},
            "stateDirectory": "{state_directory}",
            "maxInplaceDeltaBytes": 10000,
            "activation": {{ "activate": "{activate}", "currentPath": "{current}" }},
            "metrics": {{
                "textfile": "/nonexistent-nixdeploy-dir/collector/nixdeploy.prom",
                "pushUrl": "http://127.0.0.1:1/metrics"
            }}
        }}"#,
        url = MANIFEST_URL,
        key = fixture.public_key,
        state_directory = receiver_state.display(),
        activate = sh(&format!("printf %s $0 > {}", state)),
        current = sh(&format!("cat {}", state)),
    );
    let config = fixture.dir.join("config.json");
    fs::write(&config, json).expect("write config");

    let outcome = nixdeploy::receive::run_with(&config, &fixture.env(None));

    assert_eq!(
        outcome,
        Outcome::Converged {
            from: OLD_PATH.to_string(),
            to: NEW_PATH.to_string(),
        },
        "a machine that converged and then could not report it has still converged"
    );
    assert_eq!(fixture.current_path(), NEW_PATH);
}

#[test]
fn an_unparseable_config_fails_before_touching_the_machine() {
    let fixture = Fixture::new("bad-config", None);
    let config = fixture.dir.join("config.json");
    fs::write(&config, "{ not json").expect("write config");

    let outcome = nixdeploy::receive::run_with(&config, &fixture.env(None));

    assert!(
        matches!(
            outcome,
            Outcome::Failed {
                stage: Stage::Config,
                ..
            }
        ),
        "got {:?}",
        outcome
    );
    assert_eq!(fixture.current_path(), OLD_PATH);
}

#[test]
fn the_receiver_verifies_against_the_key_it_was_given_not_the_one_that_signed() {
    let fixture = Fixture::new("wrong-key", None);
    let other = SigningKey::from_bytes(&[99u8; 32]);
    let state = fixture.state.display().to_string();
    let receiver_state = fixture.dir.join("wrong-key-state");
    let json = format!(
        r#"{{
            "manifest": {{ "url": "{url}", "publicKey": "other:{key}" }},
            "plane": {{ "name": "nixos", "backend": "nixos" }},
            "stateDirectory": "{state_directory}",
            "activation": {{ "activate": "{activate}", "currentPath": "{current}" }}
        }}"#,
        url = MANIFEST_URL,
        key = BASE64.encode(other.verifying_key().to_bytes()),
        state_directory = receiver_state.display(),
        activate = sh(&format!("printf %s $0 > {}", state)),
        current = sh(&format!("cat {}", state)),
    );
    let config = fixture.dir.join("config-wrong-key.json");
    fs::write(&config, json).expect("write config");

    let outcome = nixdeploy::receive::run_with(&config, &fixture.env(None));

    match &outcome {
        Outcome::Failed { stage, detail } => {
            assert_eq!(*stage, Stage::Manifest);
            assert!(detail.contains("signature"), "detail was: {}", detail);
        }
        other => panic!("want a Manifest failure, got {:?}", other),
    }
    assert_eq!(fixture.current_path(), OLD_PATH);
}

/// `delta::compute` is exercised through the receiver above; this pins the one property the
/// pipeline depends on and no single stage owns: what the receiver sizes is the target from
/// the manifest, walked from the target itself, pruned at what this store already holds.
#[test]
fn the_delta_the_receiver_sizes_is_the_manifest_target_pruned_at_the_local_store() {
    let fixture = Fixture::new("delta-shape", None);
    let env = fixture.env(None);
    let sources = env
        .delta_sources(
            &nixdeploy::receive::load_config(&fixture.config(Some(10_000), None, false))
                .expect("config"),
        )
        .expect("sources");

    let delta: Delta =
        nixdeploy::delta::compute(NEW_PATH, sources.store.as_ref(), sources.narinfo.as_ref())
            .expect("compute");

    assert_eq!(delta.bytes, 500);
    assert_eq!(
        delta.missing,
        vec![NEW_PATH.to_string(), DEP_PATH.to_string()]
    );
    assert!(
        !delta.missing.contains(&OLD_PATH.to_string()),
        "a path already in the store costs nothing and must not be listed"
    );
}

/// Guards the one file path the module surface has to render correctly.
#[test]
fn the_default_config_path_is_the_documented_one() {
    assert_eq!(
        nixdeploy::receive::parse_args(&[]),
        Path::new("/etc/nixdeploy/config.json")
    );
}
