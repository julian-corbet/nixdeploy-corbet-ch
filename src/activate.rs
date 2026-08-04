//! Shells out to the three commands a backend adapter contributes (`activate`,
//! `currentPath`, `rollback` -- see `modules/default.nix`'s `activationAdapter`) and to the
//! operator's own health-gate commands (`nixdeploy.receiver.healthGate`).
//!
//! Every command here is a full command line rendered by Nix -- an absolute store path,
//! sometimes followed by fixed arguments -- tokenized and exec'd directly, never through a
//! shell: a Nix store path never contains a literal space, so simple whitespace/quote
//! tokenizing is all this ever needs, and skipping the shell removes an entire class of
//! quoting bugs a shell would otherwise let a misconfigured command line hide.
//!
//! The one rule everything in this module is built around is spelled out on
//! `activationAdapter.activate` in `modules/default.nix`: a switch command's exit code
//! cannot be trusted to mean "the machine is now running this closure," because a backend
//! whose tool returns non-zero for an unrelated reason (some unit failed to restart) while
//! the configuration applied fine would report a healthy activation as a failure, and a
//! tool that returns zero without having applied anything reports the opposite. So the exit
//! code of `activate` is captured here purely as a diagnostic detail, never as the verdict
//! -- the verdict always comes from re-reading `currentPath` afterward and comparing it to
//! the target this receiver actually asked for.

use std::fmt;
use std::process::Command;

use serde::Deserialize;

/// Mirrors `modules/default.nix`'s `activationAdapter` submodule field-for-field --
/// deserialized straight out of the receiver's on-disk config (see `main.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivationAdapter {
    pub activate: String,
    pub current_path: String,
    #[serde(default)]
    pub rollback: Option<String>,
}

#[derive(Debug)]
pub enum AdapterError {
    /// The command line itself could not be split into arguments (e.g. an unterminated
    /// quote) -- a misconfiguration, never seen from a Nix-rendered config.
    Tokenize(String, String),
    /// The command could not even be spawned (missing binary, no exec permission, ...).
    Spawn(String, String),
    /// The command ran, exited zero, but printed nothing usable as a store path -- an
    /// adapter that does not honour `currentPath`'s contract ("printing the store path ...
    /// with no trailing content").
    EmptyOutput(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdapterError::Tokenize(cmd, e) => write!(f, "tokenizing command {:?}: {}", cmd, e),
            AdapterError::Spawn(cmd, e) => write!(f, "running command {:?}: {}", cmd, e),
            AdapterError::EmptyOutput(cmd) => {
                write!(f, "command {:?} printed no output", cmd)
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// What actually happened when a command whose exit code is NOT trusted (`activate`,
/// `rollback`) was run. Kept around purely so a `Failed` outcome's `detail` can quote it --
/// nothing in this module ever branches on `success`, only on the re-read `currentPath` that
/// follows.
#[derive(Debug, Clone)]
pub enum RawCommandResult {
    Ran {
        exit_code: Option<i32>,
        success: bool,
    },
    CouldNotRun(String),
}

impl fmt::Display for RawCommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawCommandResult::Ran {
                exit_code: Some(code),
                success,
            } => write!(
                f,
                "exited {} ({})",
                code,
                if *success { "success" } else { "non-zero" }
            ),
            // No exit_code at all means the process was killed by a signal rather than
            // exiting normally -- still worth spelling out explicitly rather than folding
            // into the same message as a normal exit.
            RawCommandResult::Ran {
                exit_code: None,
                success,
            } => write!(
                f,
                "terminated by signal ({})",
                if *success { "success" } else { "non-zero" }
            ),
            RawCommandResult::CouldNotRun(detail) => write!(f, "could not run: {}", detail),
        }
    }
}

/// Runs `adapter.currentPath` and returns the store path it printed. This is the ONLY
/// ground truth this crate ever trusts for "what is this machine actually running" -- never
/// a value remembered from an earlier run, and never the target this receiver merely asked
/// for (see `modules/default.nix`'s doc on why `currentPath` is asked of the machine rather
/// than recorded by anyone else).
pub fn current_path(adapter: &ActivationAdapter) -> Result<String, AdapterError> {
    run_capturing(&adapter.current_path)
}

/// The result of running `activate` followed by a fresh `currentPath` read.
#[derive(Debug, Clone)]
pub struct ActivationAttempt {
    pub raw: RawCommandResult,
    /// Whether `currentPath`, re-read after `activate` ran, equals `target`. This -- and
    /// only this -- is what "did activation work" means in this crate.
    pub became: bool,
    pub observed_path: String,
}

/// Runs `adapter.activate <target>`, then unconditionally re-reads `currentPath` and
/// compares it to `target`, regardless of what the `activate` command's own exit status
/// said. See the module doc for why the exit code is never consulted for `became`.
pub fn activate(
    adapter: &ActivationAdapter,
    target: &str,
) -> Result<ActivationAttempt, AdapterError> {
    let argv = tokenize(&adapter.activate)
        .map_err(|e| AdapterError::Tokenize(adapter.activate.clone(), e))?;
    let (bin, base_args) = argv.split_first().ok_or_else(|| {
        AdapterError::Tokenize(adapter.activate.clone(), "empty command".to_string())
    })?;
    let mut args: Vec<String> = base_args.to_vec();
    args.push(target.to_string());

    let raw = run_raw(bin, &args);
    let observed_path = current_path(adapter)?;
    let became = observed_path == target;

    Ok(ActivationAttempt {
        raw,
        became,
        observed_path,
    })
}

/// The result of running `rollback`, plus a fresh `currentPath` read so the caller can see
/// what the machine actually ended up on -- rollback's own exit code is exactly as
/// untrustworthy as `activate`'s, for the same reason.
#[derive(Debug, Clone)]
pub struct RollbackAttempt {
    pub raw: RawCommandResult,
    pub observed_path: String,
}

/// Runs `adapter.rollback` if this backend has one. `Ok(None)` means it does not --
/// `modules/default.nix` documents this as a legitimate answer, not a missing feature: the
/// receiver then reports a failed activation it could not undo, rather than pretending it
/// did.
pub fn rollback(adapter: &ActivationAdapter) -> Result<Option<RollbackAttempt>, AdapterError> {
    let Some(cmd) = &adapter.rollback else {
        return Ok(None);
    };
    let argv = tokenize(cmd).map_err(|e| AdapterError::Tokenize(cmd.clone(), e))?;
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| AdapterError::Tokenize(cmd.clone(), "empty command".to_string()))?;

    let raw = run_raw(bin, args);
    let observed_path = current_path(adapter)?;
    Ok(Some(RollbackAttempt { raw, observed_path }))
}

/// The result of running the full `healthGate` list. Stops at the first command that is not
/// a plain pass, since `modules/default.nix` requires ALL of them to exit zero -- one
/// failure (of either kind) already means the gate did not pass.
#[derive(Debug, Clone)]
pub enum HealthGateOutcome {
    Passed,
    /// A health-gate command could not even be run. Deliberately NOT the same shape as
    /// `Failed` below -- see `outcome::Stage::HealthCheckUnavailable`'s doc for the incident
    /// this distinction exists to prevent.
    Unavailable {
        command: String,
        detail: String,
    },
    /// A health-gate command ran and exited non-zero: the machine is genuinely considered
    /// unhealthy.
    Failed {
        command: String,
        detail: String,
    },
}

/// Runs each command in `commands` in order. An empty list passes vacuously -- "all of zero
/// commands exited zero" is true, and `modules/default.nix` does not require at least one
/// health check to be configured.
pub fn run_health_gate(commands: &[String]) -> HealthGateOutcome {
    for cmd in commands {
        let argv = match tokenize(cmd) {
            Ok(a) => a,
            Err(e) => {
                return HealthGateOutcome::Unavailable {
                    command: cmd.clone(),
                    detail: format!("could not tokenize command line: {}", e),
                }
            }
        };
        let Some((bin, args)) = argv.split_first() else {
            return HealthGateOutcome::Unavailable {
                command: cmd.clone(),
                detail: "empty command".to_string(),
            };
        };

        match Command::new(bin).args(args).status() {
            // The process never started at all -- missing binary, bad interpreter path, no
            // exec permission. This is a BROKEN PROBE, not an unhealthy machine.
            Err(e) => {
                return HealthGateOutcome::Unavailable {
                    command: cmd.clone(),
                    detail: e.to_string(),
                }
            }
            Ok(status) if status.success() => continue,
            Ok(status) => {
                return HealthGateOutcome::Failed {
                    command: cmd.clone(),
                    detail: format!("exited {}", status),
                }
            }
        }
    }
    HealthGateOutcome::Passed
}

fn run_raw(bin: &str, args: &[String]) -> RawCommandResult {
    match Command::new(bin).args(args).status() {
        Ok(status) => RawCommandResult::Ran {
            exit_code: status.code(),
            success: status.success(),
        },
        Err(e) => RawCommandResult::CouldNotRun(e.to_string()),
    }
}

/// Runs a command line with no extra arguments and returns its trimmed stdout, requiring a
/// clean exit and non-empty output. Used only for `currentPath`, whose contract (per
/// `modules/default.nix`) is exactly this: print the path, nothing else, and nothing about
/// it is allowed to be ambiguous the way `activate`'s exit code is.
fn run_capturing(cmd: &str) -> Result<String, AdapterError> {
    let argv = tokenize(cmd).map_err(|e| AdapterError::Tokenize(cmd.to_string(), e))?;
    let (bin, args) = argv
        .split_first()
        .ok_or_else(|| AdapterError::Tokenize(cmd.to_string(), "empty command".to_string()))?;

    let output = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| AdapterError::Spawn(cmd.to_string(), e.to_string()))?;
    if !output.status.success() {
        return Err(AdapterError::Spawn(
            cmd.to_string(),
            format!("exited {}", output.status),
        ));
    }
    let trimmed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if trimmed.is_empty() {
        return Err(AdapterError::EmptyOutput(cmd.to_string()));
    }
    Ok(trimmed)
}

/// Minimal shell-word splitting (whitespace-separated, single/double quotes respected, no
/// globbing/expansion/escapes beyond that) -- Nix store paths never contain literal spaces,
/// so this only ever needs to split `"<path> arg1 arg2 ..."`.
fn tokenize(s: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for c in s.chars() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
        } else if in_double {
            if c == '"' {
                in_double = false;
            } else {
                cur.push(c);
            }
        } else if c == '\'' {
            in_single = true;
        } else if c == '"' {
            in_double = true;
        } else if c == ' ' || c == '\t' {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if in_single || in_double {
        return Err(format!("unterminated quote in command line {:?}", s));
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command line that runs `body` through the system's own `/bin/sh -c`, for tests that
    /// need a REAL process to spawn and a REAL exit status to observe -- the whole point of
    /// this module is what happens when a REAL command's exit code disagrees with reality,
    /// so a fake/mock command would test nothing.
    ///
    /// Deliberately NOT a freshly-written executable temp script: writing a script file,
    /// chmod'ing it, and `execve`-ing it moments later hit a real, reproducible `ETXTBSY`
    /// ("text file busy") during this crate's own development, on a machine busy enough that
    /// the write-close-exec sequence for a just-created file occasionally raced against
    /// itself. `/bin/sh` is a long-lived binary nothing here ever writes to, so exec'ing it
    /// (with the actual test body passed as its `-c` argument) cannot hit that race by
    /// construction -- there is no freshly-written executable in the loop at all. Wrapped in
    /// single quotes to satisfy THIS crate's own `tokenize` (a real shell's escaping rules do
    /// not apply here, and are not needed: no body used below contains a quote character).
    fn sh(body: &str) -> String {
        format!("/bin/sh -c '{}'", body)
    }

    #[test]
    fn tokenize_matches_house_convention() {
        let cases: &[(&str, &[&str])] = &[
            ("/bin/foo", &["/bin/foo"]),
            ("/bin/foo bar baz", &["/bin/foo", "bar", "baz"]),
            ("/bin/foo 'has space'", &["/bin/foo", "has space"]),
            ("  /bin/foo   bar  ", &["/bin/foo", "bar"]),
        ];
        for (input, want) in cases {
            assert_eq!(&tokenize(input).unwrap(), want, "tokenize({:?})", input);
        }
        assert!(tokenize("/bin/foo \"unterminated").is_err());
    }

    #[test]
    fn activate_ignores_exit_code_and_trusts_reread_current_path_only() {
        // The exact incident `modules/default.nix` warns about: `activate` exits non-zero
        // (an "unrelated unit failed") even though the switch actually applied, i.e.
        // currentPath now reports the target. This must be reported as `became = true`.
        let adapter = ActivationAdapter {
            activate: sh("exit 1"),
            current_path: sh("echo target-path"),
            rollback: None,
        };
        let attempt = activate(&adapter, "target-path").expect("activate should run");
        assert!(
            attempt.became,
            "currentPath reported the target; a non-zero activate exit must not override that"
        );
        assert_eq!(attempt.observed_path, "target-path");
        assert!(matches!(
            attempt.raw,
            RawCommandResult::Ran { success: false, .. }
        ));
    }

    #[test]
    fn activate_exit_zero_does_not_imply_became_target() {
        // The opposite incident: `activate` exits zero but currentPath still reports the
        // OLD path (nothing actually applied). This must be reported as `became = false`.
        let adapter = ActivationAdapter {
            activate: sh("exit 0"),
            current_path: sh("echo old-path"),
            rollback: None,
        };
        let attempt = activate(&adapter, "new-path").expect("activate should run");
        assert!(!attempt.became);
        assert_eq!(attempt.observed_path, "old-path");
    }

    #[test]
    fn health_gate_distinguishes_could_not_run_from_ran_and_failed() {
        let missing_binary = "/nonexistent/nixdeploy-test-binary-xyz".to_string();
        match run_health_gate(std::slice::from_ref(&missing_binary)) {
            HealthGateOutcome::Unavailable { .. } => {}
            other => panic!("want Unavailable for a missing binary, got {:?}", other),
        }

        let failing_cmd = sh("exit 1");
        match run_health_gate(std::slice::from_ref(&failing_cmd)) {
            HealthGateOutcome::Failed { .. } => {}
            other => panic!(
                "want Failed for a command that ran and exited 1, got {:?}",
                other
            ),
        }

        let passing_cmd = sh("exit 0");
        match run_health_gate(std::slice::from_ref(&passing_cmd)) {
            HealthGateOutcome::Passed => {}
            other => panic!("want Passed for a command that exits 0, got {:?}", other),
        }
    }

    #[test]
    fn empty_health_gate_passes_vacuously() {
        assert!(matches!(run_health_gate(&[]), HealthGateOutcome::Passed));
    }

    #[test]
    fn current_path_rejects_empty_output() {
        let adapter = ActivationAdapter {
            activate: "true".to_string(),
            current_path: sh("true"),
            rollback: None,
        };
        let err = current_path(&adapter).unwrap_err();
        assert!(matches!(err, AdapterError::EmptyOutput(_)));
    }

    #[test]
    fn rollback_none_when_not_configured() {
        let adapter = ActivationAdapter {
            activate: "true".to_string(),
            current_path: "true".to_string(),
            rollback: None,
        };
        assert!(matches!(rollback(&adapter), Ok(None)));
    }
}
