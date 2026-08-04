//! nixdeploy's receiver binary. A single run reads the manifest naming this machine's
//! target closure, decides whether it can become that closure safely, and, if so, becomes
//! it -- producing exactly one `outcome::Outcome`, printed as JSON on stdout, every time
//! (see `outcome.rs` for why that "exactly one" is the whole reason this crate is written
//! the way it is).
//!
//! This binary is entirely Nix-unaware in the same sense nixnet's daemon is: it reads one
//! JSON config file and shells out to whatever commands that config names. It never
//! evaluates a Nix expression, never builds a derivation, and never links against Nix as a
//! library -- see `README.md`'s "Not a builder."

mod activate;
mod delta;
mod manifest;
mod outcome;

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

use outcome::{Outcome, RefusedReason, Stage};

/// The receiver's on-disk config. Field names and nesting deliberately mirror
/// `nixdeploy.receiver`'s own option paths in `modules/default.nix` one-for-one
/// (`manifest.url`, `manifest.publicKey`, `maxInplaceDeltaBytes`, `activation.*`,
/// `healthGate`) so whatever Nix code renders this file is a direct, mechanical
/// transcription of that module's config, not a second schema someone has to keep in sync
/// with the first by hand.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReceiverConfig {
    manifest: ManifestConfig,
    #[serde(default)]
    max_inplace_delta_bytes: Option<u64>,
    activation: activate::ActivationAdapter,
    #[serde(default)]
    health_gate: Vec<String>,
    /// The `nix` binary used for local store queries (`delta::NixStore`) and for
    /// discovering this machine's own substituters (see `substituters_from_nix_config`
    /// below). Defaults to a bare `"nix"` PATH lookup so a hand-written config does not need
    /// to know where Nix happens to be installed; a Nix-rendered config can pin it to a
    /// store path like every other command in this file.
    #[serde(default = "default_nix_binary")]
    nix_binary: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestConfig {
    url: String,
    public_key: String,
}

fn default_nix_binary() -> String {
    "nix".to_string()
}

fn main() -> ExitCode {
    let config_path = parse_args();
    let outcome = run(&config_path);
    println!("{}", outcome.serialize());
    if outcome.is_error() {
        // Only a genuine failure is noisy on stderr -- AlreadyCurrent, Refused and
        // Reimaged are all legitimate outcomes an operator watching logs should not have
        // to triage, so they never print anything beyond the JSON line above.
        eprintln!("nixdeploy: run failed, see the JSON on stdout for which stage");
    }
    ExitCode::from(outcome.exit_code())
}

/// Hand-rolled rather than pulling in a CLI-parsing crate, matching nixnetd's own
/// `-config PATH` convention: this binary only ever has this one flag.
fn parse_args() -> PathBuf {
    let mut config_path = PathBuf::from("/etc/nixdeploy/config.json");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-config" | "--config" => {
                if let Some(v) = args.next() {
                    config_path = PathBuf::from(v);
                }
            }
            s if s.starts_with("-config=") || s.starts_with("--config=") => {
                let v = s.split_once('=').map(|(_, v)| v).unwrap_or_default();
                config_path = PathBuf::from(v);
            }
            _ => {}
        }
    }
    config_path
}

/// The whole pipeline, start to finish, returning at the first point that determines the
/// run's outcome. Every early return here is a POSITIVE observation backing the `Outcome`
/// it constructs -- see `outcome.rs`'s module doc for why that matters.
fn run(config_path: &Path) -> Outcome {
    let cfg = match load_config(config_path) {
        Ok(c) => c,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Config,
                detail: format!("{}: {}", config_path.display(), detail),
            }
        }
    };

    let hostname = match local_hostname() {
        Ok(h) => h,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Manifest,
                detail: format!("determining this machine's own hostname: {}", detail),
            }
        }
    };

    let target =
        match manifest::fetch_and_verify(&cfg.manifest.url, &cfg.manifest.public_key, &hostname) {
            Ok(t) => t,
            Err(e) => {
                return Outcome::Failed {
                    stage: Stage::Manifest,
                    detail: e.to_string(),
                }
            }
        };

    let current = match activate::current_path(&cfg.activation) {
        Ok(p) => p,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Activate,
                detail: format!("reading currentPath before activating: {}", e),
            }
        }
    };

    if current == target.store_path {
        return Outcome::AlreadyCurrent { rev: current };
    }

    let substituters = match substituters_from_nix_config(&cfg.nix_binary) {
        Ok(s) => s,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Delta,
                detail,
            }
        }
    };

    let store = delta::NixStore {
        nix_binary: cfg.nix_binary.clone(),
    };
    let narinfo_source = delta::HttpNarinfoSource {
        substituters,
        store_dir: delta::DEFAULT_STORE_DIR.to_string(),
    };

    let computed = match delta::compute(&target.store_path, &store, &narinfo_source) {
        Ok(d) => d,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Delta,
                detail: e.to_string(),
            }
        }
    };

    if let Some(ceiling) = cfg.max_inplace_delta_bytes {
        if delta::exceeds_ceiling(computed.bytes, Some(ceiling)) {
            return Outcome::Refused {
                reason: RefusedReason::DeltaExceedsCeiling,
                bytes: computed.bytes,
                ceiling,
            };
        }
    }

    let attempt = match activate::activate(&cfg.activation, &target.store_path) {
        Ok(a) => a,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Activate,
                detail: format!("running activate: {}", e),
            }
        }
    };

    if !attempt.became {
        return Outcome::Failed {
            stage: Stage::Activate,
            detail: format!(
                "activate command {}, but currentPath afterward is {:?}, want {:?} -- \
                 the activate command's own exit code is never trusted for this, see \
                 modules/default.nix's activationAdapter.activate",
                attempt.raw, attempt.observed_path, target.store_path
            ),
        };
    }

    match activate::run_health_gate(&cfg.health_gate) {
        activate::HealthGateOutcome::Passed => Outcome::Converged {
            from: current,
            to: target.store_path,
        },

        // Deliberately NOT rolling back: a probe that could not run at all says nothing
        // about whether the new closure is healthy, and reverting healthy work because a
        // health-check command had a typo is the exact incident `outcome::Stage`'s doc
        // exists to prevent.
        activate::HealthGateOutcome::Unavailable { command, detail } => Outcome::Failed {
            stage: Stage::HealthCheckUnavailable,
            detail: format!(
                "health gate command {:?} could not run: {} -- closure {} left active, unverified",
                command, detail, target.store_path
            ),
        },

        activate::HealthGateOutcome::Failed { command, detail } => {
            match activate::rollback(&cfg.activation) {
                Ok(Some(rb)) => Outcome::Failed {
                    stage: Stage::HealthCheckFailed,
                    detail: format!(
                        "health gate command {:?} failed ({}); rolled back, currentPath now {:?} \
                         (rollback command {})",
                        command, detail, rb.observed_path, rb.raw
                    ),
                },
                Ok(None) => Outcome::Failed {
                    stage: Stage::HealthCheckFailed,
                    detail: format!(
                        "health gate command {:?} failed ({}); no rollback command configured for \
                         this backend, closure {} left active unhealthy",
                        command, detail, target.store_path
                    ),
                },
                Err(e) => Outcome::Failed {
                    stage: Stage::Rollback,
                    detail: format!(
                        "health gate command {:?} failed ({}); rollback itself could not run: {}",
                        command, detail, e
                    ),
                },
            }
        }
    }
}

fn load_config(path: &Path) -> Result<ReceiverConfig, String> {
    let data = std::fs::read_to_string(path).map_err(|e| format!("reading config: {}", e))?;
    serde_json::from_str(&data).map_err(|e| format!("parsing config: {}", e))
}

/// This machine's own hostname -- the key this receiver looks itself up by in the
/// manifest's fleet-wide `hosts` map (see `manifest.rs`'s "Wire shape"). A direct
/// `gethostname(2)` call rather than shelling out to a `hostname` binary or reading
/// `networking.hostName` out of some rendered config: it needs no PATH lookup, no extra
/// process, and no assumption that this crate's own config file happens to carry a copy of
/// a fact the kernel already has.
fn local_hostname() -> Result<String, String> {
    // 256 bytes comfortably covers POSIX's HOST_NAME_MAX (64) with room to spare; a
    // hostname that still doesn't fit is not a case worth a resizing retry loop for.
    let mut buf = vec![0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..len].to_vec())
        .map_err(|e| format!("hostname is not valid UTF-8: {}", e))
}

/// This machine's own substituters, as Nix itself already has them configured
/// (`nix.settings.substituters` or equivalent -- a standing, system-wide fact this module
/// deliberately does not duplicate with a `nixdeploy`-specific option of its own). Read via
/// `nix show-config --json` rather than parsing `nix.conf` by hand, so this always sees the
/// same merged view of every config file and command-line override Nix itself would use.
fn substituters_from_nix_config(nix_binary: &str) -> Result<Vec<String>, String> {
    let output = Command::new(nix_binary)
        .args([
            "--extra-experimental-features",
            "nix-command",
            "show-config",
            "--json",
        ])
        .output()
        .map_err(|e| format!("running {} show-config: {}", nix_binary, e))?;
    if !output.status.success() {
        return Err(format!(
            "{} show-config exited {}",
            nix_binary, output.status
        ));
    }

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parsing {} show-config output: {}", nix_binary, e))?;
    let substituters = parsed
        .get("substituters")
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "{} show-config output has no substituters.value array",
                nix_binary
            )
        })?;

    Ok(substituters
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_from_module_shaped_json() {
        let json = r#"{
            "manifest": { "url": "https://example.org/manifest.json", "publicKey": "k:AAAA" },
            "maxInplaceDeltaBytes": 500000000,
            "activation": {
                "activate": "/nix/store/xxx-switch/bin/switch",
                "currentPath": "/nix/store/xxx-switch/bin/current",
                "rollback": null
            },
            "healthGate": ["/nix/store/xxx-check/bin/check"]
        }"#;
        let cfg: ReceiverConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.manifest.url, "https://example.org/manifest.json");
        assert_eq!(cfg.max_inplace_delta_bytes, Some(500_000_000));
        assert_eq!(cfg.health_gate.len(), 1);
        assert_eq!(
            cfg.nix_binary, "nix",
            "nixBinary should default when absent"
        );
        assert!(cfg.activation.rollback.is_none());
    }

    #[test]
    fn missing_config_file_is_a_config_stage_failure() {
        let outcome = run(Path::new("/nonexistent/nixdeploy-config.json"));
        assert!(matches!(
            outcome,
            Outcome::Failed {
                stage: Stage::Config,
                ..
            }
        ));
        assert!(outcome.is_error());
    }
}
