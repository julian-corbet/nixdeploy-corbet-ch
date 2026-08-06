//! `nixdeploy receive`: one run reads the manifest naming this receiver's selected plane,
//! decides whether it can become that closure safely, and, if so, becomes it -- producing
//! exactly one `outcome::Outcome` every time (see `outcome.rs` for why that "exactly one" is
//! the whole reason this crate is written the way it is).
//!
//! This half of the binary is entirely Nix-unaware in the same sense nixnet's daemon is: it
//! reads one JSON config file and shells out to whatever commands that config names. It never
//! evaluates a Nix expression, never builds a derivation, and never links against Nix as a
//! library -- see `README.md`'s "Not a builder."
//!
//! # Everything the outside world does is behind `Env`
//!
//! The pipeline this module implements -- manifest, delta, activation -- used to construct its
//! own HTTP client and its own `nix`-backed store query inline, which meant the pipeline
//! AS A WHOLE could not be exercised at all: each stage had unit tests against its own fakes,
//! and the wiring between them had none. That is the shape of bug that survives a green test
//! suite, because every part works and the assembly is what is wrong. `Env` exists so the
//! whole run -- signature verification through delta sizing through activation and the health
//! gate -- can be driven end to end over bytes a test produced, with no socket and no store.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::activate;
use crate::atomicfile;
use crate::delta::{self, LocalStore, NarinfoSource};
use crate::manifest::{self, Backend, Fetcher, HttpFetcher};
use crate::metrics::{self, MetricsConfig, RunReport};
use crate::outcome::{Outcome, RefusedReason, Stage};

/// The receiver's on-disk config. Policy field names and nesting deliberately mirror
/// `nixdeploy.receiver`'s own option paths in `modules/default.nix` (`manifest.url`,
/// `manifest.publicKey`, `maxInplaceDeltaBytes`, `activation.*`, `healthGate`, `metrics.*`)
/// so whatever Nix code renders this file is a direct, mechanical transcription rather than
/// a second policy schema. `stateDirectory` is the scheduler-owned exception: its default
/// mirrors the systemd adapter's `StateDirectory=nixdeploy`, while another scheduler can
/// render the service-owned location it declares.
///
/// Unknown fields are tolerated rather than rejected. This file is rendered by a module that
/// may legitimately be newer than the binary reading it (a mixed fleet mid-rollout), and the
/// cost of the two failure modes is not symmetric: ignoring a field this binary does not
/// understand loses one feature on one machine, while refusing the config outright loses the
/// machine's entire path back to convergence -- including its ability to receive the fix.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverConfig {
    pub manifest: ManifestConfig,
    /// The one named plane this receiver instance owns. The activation adapter below is
    /// configured for exactly this backend and identity; both are cross-checked against the
    /// signed target before any current-path or activation command is run.
    pub plane: ReceiverPlane,
    /// Service-owned persistent state. The systemd adapter declares this as
    /// `/var/lib/nixdeploy`; spelling it out in the JSON contract lets other schedulers and
    /// unprivileged receiver instances provide their own conventional location without
    /// borrowing a login account's home.
    #[serde(default = "default_state_directory")]
    pub state_directory: PathBuf,
    #[serde(default)]
    pub max_inplace_delta_bytes: Option<u64>,
    pub activation: activate::ActivationAdapter,
    #[serde(default)]
    pub health_gate: Vec<String>,
    /// The `nix` binary used for local store queries (`delta::NixStore`) and for
    /// discovering this machine's own substituters (see `substituters_from_nix_config`
    /// below). Defaults to a bare `"nix"` PATH lookup so a hand-written config does not need
    /// to know where Nix happens to be installed; a Nix-rendered config can pin it to a
    /// store path like every other command in this file.
    #[serde(default = "default_nix_binary")]
    pub nix_binary: String,
    /// Command asking this machine's provider to replace it with the image the manifest
    /// names, invoked when -- and only when -- a delta comes back over the ceiling. Receives
    /// the image reference as its single argument, matching
    /// `provisioningAdapter.reimage`'s contract in `modules/default.nix`.
    ///
    /// `null` is a legitimate and complete answer: the receiver then refuses and stops,
    /// which is the correct behaviour for a machine whose operator has decided it is never
    /// replaced automatically. See `route_over_ceiling` for what the configured case can and
    /// cannot honestly claim.
    #[serde(default)]
    pub reimage: Option<String>,
    /// Where this run's outcome is reported, if anywhere. Off unless configured.
    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestConfig {
    pub url: String,
    pub public_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiverPlane {
    pub name: String,
    pub backend: Backend,
    #[serde(default)]
    pub identity: Option<String>,
}

fn default_nix_binary() -> String {
    "nix".to_string()
}

fn default_state_directory() -> PathBuf {
    PathBuf::from("/var/lib/nixdeploy")
}

const REJECTED_TARGET_STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RejectedTargetState {
    version: u32,
    plane: String,
    target: String,
    rejected_at: u64,
}

/// Everything a run needs from outside this process. One trait rather than four constructor
/// arguments so a test supplies a whole world in one value, and so adding a dependency later
/// is a change to this trait rather than to every call site.
pub trait Env {
    /// This machine's own hostname -- the first key it looks up before its configured plane.
    fn hostname(&self) -> Result<String, String>;
    /// Where manifest bytes come from.
    fn fetcher(&self) -> &dyn Fetcher;
    /// The two things `delta::compute` asks about the world: what this store already has,
    /// and what the substituters say the rest costs.
    fn delta_sources(&self, cfg: &ReceiverConfig) -> Result<DeltaSources, String>;
    /// UNIX seconds, for the metrics timestamp. Injected so a test can pin it.
    fn now_unix(&self) -> u64;
}

pub struct DeltaSources {
    pub store: Box<dyn LocalStore>,
    pub narinfo: Box<dyn NarinfoSource>,
}

/// The real world: a real hostname, real HTTPS, and a real `nix` on this machine.
pub struct RealEnv {
    fetcher: HttpFetcher,
}

impl RealEnv {
    pub fn new() -> Self {
        RealEnv {
            fetcher: HttpFetcher,
        }
    }
}

impl Default for RealEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Env for RealEnv {
    fn hostname(&self) -> Result<String, String> {
        local_hostname()
    }

    fn fetcher(&self) -> &dyn Fetcher {
        &self.fetcher
    }

    fn delta_sources(&self, cfg: &ReceiverConfig) -> Result<DeltaSources, String> {
        let substituters = substituters_from_nix_config(&cfg.nix_binary)?;
        Ok(DeltaSources {
            store: Box::new(delta::NixStore {
                nix_binary: cfg.nix_binary.clone(),
            }),
            narinfo: Box::new(delta::HttpNarinfoSource {
                substituters,
                store_dir: delta::DEFAULT_STORE_DIR.to_string(),
            }),
        })
    }

    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            // A clock before the epoch is a machine whose time is unusable; 0 is an
            // obviously-wrong timestamp that a staleness alert fires on immediately, which
            // is the correct outcome for a machine that cannot say when anything happened.
            .unwrap_or(0)
    }
}

/// Facts about this run that the `Outcome` alone cannot carry, gathered as the run proceeds
/// so the metrics report can be assembled from whatever was actually observed -- never from
/// a default standing in for a measurement that was never taken.
#[derive(Debug, Default, Clone)]
struct Measured {
    delta_bytes: Option<u64>,
    ceiling: Option<u64>,
    reimage_owed: bool,
}

/// A run against the real world.
pub fn run(config_path: &Path) -> Outcome {
    run_with(config_path, &RealEnv::new())
}

/// A run against `env`. Always reports to whatever metrics sinks the config names, whatever
/// the outcome -- with one exception it cannot avoid: a config that could not be read names
/// no sinks either, so a run that fails there is invisible to everything except the staleness
/// alarm. That is exactly what the staleness metric exists for (`metrics.rs`), and it is why
/// it is a timestamp rather than an error counter.
pub fn run_with(config_path: &Path, env: &dyn Env) -> Outcome {
    let cfg = match load_config(config_path) {
        Ok(c) => c,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Config,
                detail: format!("{}: {}", config_path.display(), detail),
            }
        }
    };

    let mut measured = Measured {
        ceiling: cfg.max_inplace_delta_bytes,
        ..Measured::default()
    };
    let outcome = converge(&cfg, env, &mut measured);
    report(&cfg, env, &outcome, &measured);
    outcome
}

/// Emits one run's metrics. Sink failures are printed and discarded: a machine that converged
/// and then could not write a metrics file has still converged, and letting the reporting
/// path decide the reported value is how a monitoring system becomes the most fragile
/// dependency in a system built to have no single point of failure.
fn report(cfg: &ReceiverConfig, env: &dyn Env, outcome: &Outcome, measured: &Measured) {
    let errors = metrics::emit(
        &cfg.metrics,
        &RunReport {
            outcome,
            delta_bytes: measured.delta_bytes,
            ceiling: measured.ceiling,
            reimage_owed: measured.reimage_owed,
            timestamp: env.now_unix(),
        },
    );
    for error in errors {
        eprintln!(
            "nixdeploy: metrics {} -- reporting only; the run's outcome is unchanged",
            error
        );
    }
}

fn rejected_target_path(cfg: &ReceiverConfig) -> PathBuf {
    cfg.state_directory
        .join(format!("rejected-target-{}.json", cfg.plane.name))
}

/// Reads this plane's poison pin. The directory is prepared before activation rather than
/// only when a health check fails: discovering after rollback that the rejection cannot be
/// persisted would leave the next scheduled tick free to repeat the same bad activation.
fn load_rejected_target(cfg: &ReceiverConfig) -> Result<Option<RejectedTargetState>, String> {
    fs::create_dir_all(&cfg.state_directory).map_err(|e| {
        format!(
            "preparing declared state directory {}: {}",
            cfg.state_directory.display(),
            e
        )
    })?;

    let path = rejected_target_path(cfg);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {}", path.display(), e)),
    };
    let state: RejectedTargetState =
        serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {}", path.display(), e))?;
    if state.version != REJECTED_TARGET_STATE_VERSION {
        return Err(format!(
            "{} has unsupported rejected-target state version {}, want {}",
            path.display(),
            state.version,
            REJECTED_TARGET_STATE_VERSION
        ));
    }
    if state.plane != cfg.plane.name {
        return Err(format!(
            "{} belongs to plane {:?}, but this receiver owns {:?}",
            path.display(),
            state.plane,
            cfg.plane.name
        ));
    }
    Ok(Some(state))
}

fn persist_rejected_target(
    cfg: &ReceiverConfig,
    target: &str,
    rejected_at: u64,
) -> Result<(), String> {
    let state = RejectedTargetState {
        version: REJECTED_TARGET_STATE_VERSION,
        plane: cfg.plane.name.clone(),
        target: target.to_string(),
        rejected_at,
    };
    let mut bytes = serde_json::to_vec_pretty(&state)
        .map_err(|e| format!("serializing rejected-target state: {}", e))?;
    bytes.push(b'\n');
    let path = rejected_target_path(cfg);
    atomicfile::write_atomic(&path, &bytes, 0o600)
        .map_err(|e| format!("persisting poison pin {}: {}", path.display(), e))
}

fn clear_rejected_target(cfg: &ReceiverConfig) -> Result<(), String> {
    let path = rejected_target_path(cfg);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "removing stale poison pin {}: {}",
            path.display(),
            e
        )),
    }
}

/// The whole pipeline, start to finish, returning at the first point that determines the
/// run's outcome. Every early return here is a POSITIVE observation backing the `Outcome`
/// it constructs -- see `outcome.rs`'s module doc for why that matters.
fn converge(cfg: &ReceiverConfig, env: &dyn Env, measured: &mut Measured) -> Outcome {
    let hostname = match env.hostname() {
        Ok(h) => h,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Manifest,
                detail: format!("determining this machine's own hostname: {}", detail),
            }
        }
    };

    let target = match manifest::fetch_and_verify(
        env.fetcher(),
        &cfg.manifest.url,
        &cfg.manifest.public_key,
        &hostname,
        &cfg.plane.name,
    ) {
        Ok(t) => t,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Manifest,
                detail: e.to_string(),
            }
        }
    };

    if target.backend != cfg.plane.backend {
        return Outcome::Failed {
            stage: Stage::Manifest,
            detail: format!(
                "manifest host {:?} plane {:?} uses backend {:?}, but this receiver is configured for {:?}",
                hostname, cfg.plane.name, target.backend, cfg.plane.backend
            ),
        };
    }
    if target.identity != cfg.plane.identity {
        return Outcome::Failed {
            stage: Stage::Manifest,
            detail: format!(
                "manifest host {:?} plane {:?} names identity {:?}, but this receiver is configured for {:?}",
                hostname, cfg.plane.name, target.identity, cfg.plane.identity
            ),
        };
    }

    let current = match activate::current_path(&cfg.activation) {
        Ok(p) => p,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Activate,
                detail: format!("reading currentPath before activating: {}", e),
            }
        }
    };

    let rejected = match load_rejected_target(cfg) {
        Ok(state) => state,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::State,
                detail,
            }
        }
    };

    if let Some(rejected) = rejected.as_ref() {
        if rejected.target == target.store_path {
            let path = rejected_target_path(cfg);
            let current_detail = if current == target.store_path {
                "it is currently active despite that recorded rejection"
            } else {
                "the receiver left the rolled-back closure untouched"
            };
            return Outcome::Failed {
                stage: Stage::RejectedTarget,
                detail: format!(
                    "target {:?} was health-rejected at UNIX time {} and pinned in {}; {}. \
                     Publish a different immutable target, or remove that pin explicitly \
                     only after deciding the same target is safe to retry",
                    rejected.target,
                    rejected.rejected_at,
                    path.display(),
                    current_detail
                ),
            };
        }
    }

    if current == target.store_path {
        return Outcome::AlreadyCurrent { rev: current };
    }

    let sources = match env.delta_sources(cfg) {
        Ok(s) => s,
        Err(detail) => {
            return Outcome::Failed {
                stage: Stage::Delta,
                detail,
            }
        }
    };

    let computed = match delta::compute(
        &target.store_path,
        sources.store.as_ref(),
        sources.narinfo.as_ref(),
    ) {
        Ok(d) => d,
        Err(e) => {
            return Outcome::Failed {
                stage: Stage::Delta,
                detail: e.to_string(),
            }
        }
    };
    measured.delta_bytes = Some(computed.bytes);

    if delta::exceeds_ceiling(computed.bytes, cfg.max_inplace_delta_bytes) {
        let ceiling = cfg
            .max_inplace_delta_bytes
            .expect("exceeds_ceiling is never true without a ceiling");
        return route_over_ceiling(cfg, env, measured, &target, computed.bytes, ceiling);
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
        activate::HealthGateOutcome::Passed => {
            if rejected.is_some() {
                if let Err(detail) = clear_rejected_target(cfg) {
                    return Outcome::Failed {
                        stage: Stage::State,
                        detail: format!(
                            "converged from {:?} to healthy target {:?}, but could not clear \
                             the previous rejected-target state: {}",
                            current, target.store_path, detail
                        ),
                    };
                }
            }
            Outcome::Converged {
                from: current,
                to: target.store_path,
            }
        }

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
                Ok(Some(rb)) if rb.observed_path != target.store_path => {
                    if let Err(state_detail) =
                        persist_rejected_target(cfg, &target.store_path, env.now_unix())
                    {
                        return Outcome::Failed {
                            stage: Stage::State,
                            detail: format!(
                                "health gate command {:?} failed ({}); rollback moved \
                                 currentPath to {:?} (command {}), but the rejected target \
                                 could not be pinned: {}",
                                command, detail, rb.observed_path, rb.raw, state_detail
                            ),
                        };
                    }
                    Outcome::Failed {
                        stage: Stage::HealthCheckFailed,
                        detail: format!(
                            "health gate command {:?} failed ({}); rolled back, currentPath now \
                             {:?} (rollback command {}); target {:?} is pinned in {} and will \
                             not be activated again",
                            command,
                            detail,
                            rb.observed_path,
                            rb.raw,
                            target.store_path,
                            rejected_target_path(cfg).display()
                        ),
                    }
                }
                Ok(Some(rb)) => Outcome::Failed {
                    stage: Stage::Rollback,
                    detail: format!(
                        "health gate command {:?} failed ({}); rollback command {}, but \
                         currentPath is still the rejected target {:?}",
                        command, detail, rb.raw, rb.observed_path
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

/// What happens when this machine cannot become the target in place.
///
/// The refusal itself is unconditional and comes first: the numbers that produced it are
/// reported before anything else is attempted, because what follows may end this process.
/// With no reimage command configured, the refusal is also the final answer -- correct, and
/// deliberately still possible, for a machine whose operator has not chosen to have it
/// replaced automatically.
///
/// With one configured, this asks the provider to replace the machine, and here the contract
/// has to be honest about what it can observe. The process making the request is running ON
/// the machine being replaced, so there are three ways the call can end: it returns zero
/// (the provider accepted the request), it returns non-zero (the provider rejected it), or
/// this process is killed mid-call and returns nothing at all. Only the first two produce an
/// `Outcome`, and neither has seen the replaced machine -- it does not exist yet, and when it
/// does it will have no memory of the request. So `Reimaged` claims exactly one thing: a
/// replacement was asked for and the request was accepted. The claim that it LANDED belongs
/// to a later run reporting `Converged` or `AlreadyCurrent` from the replacement.
///
/// The third ending is why the refusal is reported first. A killed process prints no outcome
/// at all, so the last thing this machine ever says is whatever the metrics sinks were told
/// before the call -- a refusal, with `nixdeploy_reimage_owed` at 1 and a timestamp. If the
/// replacement never comes back, that is the record that makes it visible, and it is the same
/// record a staleness alert is already watching.
///
/// One thing this route deliberately does NOT cover: a machine too wedged to run its receiver
/// at all cannot take it, because taking it requires running. That case still needs a
/// publisher-side reimage against an unreachable target (`docs/reimage.md`), which is not
/// something this binary implements.
fn route_over_ceiling(
    cfg: &ReceiverConfig,
    env: &dyn Env,
    measured: &mut Measured,
    target: &manifest::Target,
    bytes: u64,
    ceiling: u64,
) -> Outcome {
    measured.reimage_owed = true;
    let refused = Outcome::Refused {
        reason: RefusedReason::DeltaExceedsCeiling,
        bytes,
        ceiling,
    };

    let Some(command) = cfg.reimage.as_deref() else {
        return refused;
    };

    let Some(image) = target.image.as_deref() else {
        return Outcome::Failed {
            stage: Stage::Reimage,
            detail: format!(
                "delta {} bytes exceeds ceiling {} and a reimage command is configured, but \
                 the manifest names no image for this host -- there is nothing to replace it \
                 with, and inventing an image reference is not something this receiver may do",
                bytes, ceiling
            ),
        };
    };

    // The refusal, on the record, before the call that may not return.
    report(cfg, env, &refused, measured);
    eprintln!(
        "nixdeploy: refusing to activate in place ({} bytes over a ceiling of {}); asking {:?} \
         to replace this machine with image {:?} -- this process may not survive that call",
        bytes, ceiling, command, image
    );

    match activate::run_with_argument(command, image) {
        Err(e) => Outcome::Failed {
            stage: Stage::Reimage,
            detail: format!(
                "delta {} bytes exceeds ceiling {}, and the reimage command could not be run: \
                 {} -- this machine can neither activate in place nor be replaced",
                bytes, ceiling, e
            ),
        },
        // Unlike `activate`, this exit code IS the verdict -- not because it is trustworthy,
        // but because nothing more trustworthy exists. `activate`'s exit code is distrusted
        // in favour of re-reading `currentPath`; here there is nothing to re-read, since the
        // machine that would answer is the one being replaced.
        Ok(raw) if raw.succeeded() => Outcome::Reimaged {
            image: image.to_string(),
        },
        Ok(raw) => Outcome::Failed {
            stage: Stage::Reimage,
            detail: format!(
                "delta {} bytes exceeds ceiling {}, and the reimage command {} -- the provider \
                 did not accept the replacement, so this machine is over its ceiling with no \
                 route left",
                bytes, ceiling, raw
            ),
        },
    }
}

pub fn load_config(path: &Path) -> Result<ReceiverConfig, String> {
    let data = std::fs::read_to_string(path).map_err(|e| format!("reading config: {}", e))?;
    let cfg: ReceiverConfig =
        serde_json::from_str(&data).map_err(|e| format!("parsing config: {}", e))?;
    if !cfg.state_directory.is_absolute() {
        return Err(format!(
            "stateDirectory must be an absolute service-owned path, got {:?}",
            cfg.state_directory
        ));
    }
    match cfg.plane.backend {
        Backend::HomeManager => {
            if !matches!(cfg.plane.identity.as_deref(), Some(identity) if !identity.trim().is_empty())
            {
                return Err(
                    "plane.identity is required and must be non-empty for home-manager".to_string(),
                );
            }
        }
        _ if cfg.plane.identity.is_some() => {
            return Err("plane.identity is only valid for home-manager".to_string())
        }
        _ => {}
    }
    if cfg.plane.name != cfg.plane.backend.as_str() {
        return Err(format!(
            "plane.name {:?} must equal its backend {:?}",
            cfg.plane.name,
            cfg.plane.backend.as_str()
        ));
    }
    Ok(cfg)
}

/// Hand-rolled rather than pulling in a CLI-parsing crate, matching nixnetd's own
/// `-config PATH` convention: this subcommand only ever has this one flag.
pub fn parse_args(args: &[String]) -> PathBuf {
    let mut config_path = PathBuf::from("/etc/nixdeploy/config.json");
    let mut args = args.iter();
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

/// One captured command run: whether it exited zero, and what it printed on stdout. `Err`
/// means it could not be run at all, which is a different fact from running and failing --
/// the same distinction `activate.rs` keeps for the health gate, for the same reason.
type CommandRunner<'a> = dyn Fn(&str, &[&str]) -> Result<(bool, String), String> + 'a;

/// The modern spelling. `nix show-config` still works but has been a deprecated alias since
/// Nix 2.20; it prints a deprecation warning on every invocation, and an alias that has been
/// deprecated for years is one that eventually stops existing on a machine this receiver is
/// supposed to keep converging.
const CONFIG_SHOW: [&str; 5] = [
    "--extra-experimental-features",
    "nix-command",
    "config",
    "show",
    "--json",
];

/// The pre-2.20 spelling, tried only if the modern one is not understood.
const SHOW_CONFIG: [&str; 4] = [
    "--extra-experimental-features",
    "nix-command",
    "show-config",
    "--json",
];

/// This machine's own substituters, as Nix itself already has them configured
/// (`nix.settings.substituters` or equivalent -- a standing, system-wide fact this module
/// deliberately does not duplicate with a `nixdeploy`-specific option of its own). Read from
/// Nix's own config dump rather than by parsing `nix.conf` by hand, so this always sees the
/// same merged view of every config file and command-line override Nix itself would use.
fn substituters_from_nix_config(nix_binary: &str) -> Result<Vec<String>, String> {
    substituters_with_runner(nix_binary, &|bin, args| {
        let output = Command::new(bin)
            .args(args)
            .output()
            .map_err(|e| format!("running {} {}: {}", bin, args.join(" "), e))?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    })
}

/// The version-straddling half of the above, with the command execution injected so both
/// branches are testable without needing two different Nix versions on the machine running
/// the tests.
fn substituters_with_runner(
    nix_binary: &str,
    run: &CommandRunner<'_>,
) -> Result<Vec<String>, String> {
    let (ok, stdout) = run(nix_binary, &CONFIG_SHOW)?;
    if ok {
        return parse_substituters(&stdout);
    }

    // A non-zero exit here means this Nix does not understand `config show`. Falling back is
    // not optional politeness: the alternative is a receiver that cannot size a delta at all
    // on an older Nix, which means it cannot converge, on precisely the machines least likely
    // to have been updated recently.
    let (ok, stdout) = run(nix_binary, &SHOW_CONFIG)?;
    if !ok {
        return Err(format!(
            "neither `{bin} config show --json` nor the deprecated `{bin} show-config --json` \
             exited cleanly -- cannot determine this machine's substituters",
            bin = nix_binary
        ));
    }
    parse_substituters(&stdout)
}

/// Pulls `substituters.value` out of Nix's JSON config dump. Both spellings produce the same
/// shape: every setting is an object with `value`, `defaultValue`, `description` and friends,
/// so the array is one level below the setting name rather than being it.
fn parse_substituters(json: &str) -> Result<Vec<String>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("parsing nix config JSON: {}", e))?;
    let substituters = parsed
        .get("substituters")
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "nix config JSON has no substituters.value array".to_string())?;

    Ok(substituters
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn config_parses_from_module_shaped_json() {
        let json = r#"{
            "manifest": { "url": "https://example.org/manifest.json", "publicKey": "k:AAAA" },
            "plane": { "name": "nixos", "backend": "nixos" },
            "stateDirectory": "/var/lib/nixdeploy-test",
            "maxInplaceDeltaBytes": 500000000,
            "activation": {
                "activate": "/nix/store/xxx-switch/bin/switch",
                "currentPath": "/nix/store/xxx-switch/bin/current",
                "rollback": null
            },
            "healthGate": ["/nix/store/xxx-check/bin/check"],
            "reimage": "/nix/store/xxx-provider/bin/reimage",
            "metrics": { "textfile": "/var/lib/collector/nixdeploy.prom" }
        }"#;
        let cfg: ReceiverConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.manifest.url, "https://example.org/manifest.json");
        assert_eq!(cfg.max_inplace_delta_bytes, Some(500_000_000));
        assert_eq!(cfg.state_directory, Path::new("/var/lib/nixdeploy-test"));
        assert_eq!(cfg.health_gate.len(), 1);
        assert_eq!(
            cfg.nix_binary, "nix",
            "nixBinary should default when absent"
        );
        assert!(cfg.activation.rollback.is_none());
        assert_eq!(
            cfg.reimage.as_deref(),
            Some("/nix/store/xxx-provider/bin/reimage")
        );
        assert!(cfg.metrics.is_enabled());
    }

    #[test]
    fn a_config_without_reimage_or_metrics_is_complete() {
        // Both are opt-in. A machine that has neither must still parse, or the module surface
        // would be forced to render policy this repo does not have an opinion about.
        let json = r#"{
            "manifest": { "url": "https://example.org/manifest.json", "publicKey": "k:AAAA" },
            "plane": { "name": "nixos", "backend": "nixos" },
            "activation": { "activate": "a", "currentPath": "c" }
        }"#;
        let cfg: ReceiverConfig = serde_json::from_str(json).expect("parse");
        assert!(cfg.reimage.is_none());
        assert!(!cfg.metrics.is_enabled());
        assert_eq!(cfg.max_inplace_delta_bytes, None);
        assert_eq!(
            cfg.state_directory,
            Path::new("/var/lib/nixdeploy"),
            "the default must mirror the systemd adapter's StateDirectory"
        );
    }

    #[test]
    fn a_relative_state_directory_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "nixdeploy-relative-state-config-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{
                "manifest": { "url": "https://example.org/manifest.json", "publicKey": "k:AAAA" },
                "plane": { "name": "nixos", "backend": "nixos" },
                "stateDirectory": "relative/state",
                "activation": { "activate": "a", "currentPath": "c" }
            }"#,
        )
        .expect("write config");

        let error = load_config(&path).expect_err("relative state must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            error.contains("absolute service-owned path"),
            "got: {}",
            error
        );
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

    #[test]
    fn config_path_parses_in_both_flag_forms() {
        assert_eq!(
            parse_args(&["-config".to_string(), "/etc/x.json".to_string()]),
            PathBuf::from("/etc/x.json")
        );
        assert_eq!(
            parse_args(&["--config=/etc/y.json".to_string()]),
            PathBuf::from("/etc/y.json")
        );
        assert_eq!(
            parse_args(&[]),
            PathBuf::from("/etc/nixdeploy/config.json"),
            "the default config path is part of the module contract"
        );
    }

    /// Records every argv it is asked to run, and answers from a scripted table.
    struct ScriptedNix {
        calls: RefCell<Vec<Vec<String>>>,
        config_show_ok: bool,
        show_config_ok: bool,
        json: String,
    }

    impl ScriptedNix {
        fn runner(&self) -> impl Fn(&str, &[&str]) -> Result<(bool, String), String> + '_ {
            move |_bin, args| {
                self.calls
                    .borrow_mut()
                    .push(args.iter().map(|s| s.to_string()).collect());
                let is_modern = args.contains(&"config") && args.contains(&"show");
                let ok = if is_modern {
                    self.config_show_ok
                } else {
                    self.show_config_ok
                };
                Ok((ok, if ok { self.json.clone() } else { String::new() }))
            }
        }
    }

    fn nix_config_json() -> String {
        // The shape `nix config show --json` actually prints: each setting is an object, and
        // the value is one level below the name.
        r#"{
            "substituters": {
                "aliases": [],
                "defaultValue": ["https://cache.example.org"],
                "description": "substituters",
                "value": ["https://cache.example.org", "https://cache-2.example.org"]
            },
            "cores": { "value": 0 }
        }"#
        .to_string()
    }

    #[test]
    fn the_modern_spelling_is_used_and_the_deprecated_alias_is_not_reached() {
        let nix = ScriptedNix {
            calls: RefCell::new(Vec::new()),
            config_show_ok: true,
            show_config_ok: true,
            json: nix_config_json(),
        };

        let subs = substituters_with_runner("nix", &nix.runner()).expect("parse");
        assert_eq!(
            subs,
            vec![
                "https://cache.example.org".to_string(),
                "https://cache-2.example.org".to_string()
            ]
        );

        let calls = nix.calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "a Nix that understands `config show` must never be asked the deprecated \
             `show-config`: {:?}",
            calls
        );
        assert!(
            calls[0].contains(&"config".to_string()) && calls[0].contains(&"show".to_string()),
            "{:?}",
            calls[0]
        );
        assert!(!calls[0].contains(&"show-config".to_string()));
    }

    #[test]
    fn an_older_nix_falls_back_to_show_config() {
        let nix = ScriptedNix {
            calls: RefCell::new(Vec::new()),
            config_show_ok: false,
            show_config_ok: true,
            json: nix_config_json(),
        };

        let subs = substituters_with_runner("nix", &nix.runner()).expect("parse");
        assert_eq!(subs.len(), 2, "the fallback must return real substituters");

        let calls = nix.calls.borrow();
        assert_eq!(calls.len(), 2, "{:?}", calls);
        assert!(calls[0].contains(&"show".to_string()));
        assert!(
            calls[1].contains(&"show-config".to_string()),
            "the fallback must be the deprecated alias, tried second: {:?}",
            calls[1]
        );
    }

    #[test]
    fn a_nix_that_understands_neither_spelling_is_an_error_not_an_empty_list() {
        // Silently returning zero substituters would make every narinfo fetch fail with "no
        // substituters configured", reported as a Delta failure that names the wrong cause.
        let nix = ScriptedNix {
            calls: RefCell::new(Vec::new()),
            config_show_ok: false,
            show_config_ok: false,
            json: String::new(),
        };
        let err = substituters_with_runner("nix", &nix.runner()).unwrap_err();
        assert!(err.contains("substituters"), "got: {}", err);
    }

    #[test]
    fn a_nix_that_cannot_be_spawned_at_all_is_reported_immediately() {
        let err = substituters_with_runner("nix", &|_bin, _args| {
            Err("No such file or directory".to_string())
        })
        .unwrap_err();
        assert_eq!(err, "No such file or directory");
    }

    #[test]
    fn substituters_are_read_from_the_value_field_not_the_setting() {
        assert_eq!(
            parse_substituters(&nix_config_json()).unwrap(),
            vec![
                "https://cache.example.org".to_string(),
                "https://cache-2.example.org".to_string()
            ]
        );
        assert!(parse_substituters(r#"{"cores":{"value":0}}"#).is_err());
        assert!(parse_substituters(r#"{"substituters":["a"]}"#).is_err());
        assert!(parse_substituters("not json").is_err());
    }
}
