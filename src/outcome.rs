//! The one type every run of the receiver produces, exactly once (see `receive.rs`).
//!
//! This crate exists to prevent one specific failure: a run that delivers nothing and
//! reports success. That failure is not hypothetical -- it is what happens whenever "ran to
//! completion" and "the machine actually changed" get treated as the same fact, and it is
//! how a delivery mechanism ends up green in every dashboard for months while the fleet it
//! is supposed to be converging quietly drifts. Guarding against it is impossible if the
//! return type of a run even HAS a shape that can express "ran, nothing worth reporting
//! happened, treat as fine" -- because whatever code constructs that value has already made
//! the mistake, and every caller downstream inherits it without a way to notice.
//!
//! So `Outcome` has no such shape, on purpose. There is no `Ok(())`, no bare `Success`, no
//! variant with an empty payload that a lazy or half-finished code path can hand back in
//! place of actually finding out what happened. Every variant carries the evidence for its
//! own claim: `Converged` names both endpoints of the change, `AlreadyCurrent` names the
//! revision it confirmed by re-reading `currentPath` (not the revision it assumed), `Refused`
//! carries the exact byte counts that justified refusing, `Reimaged` names the image, and
//! `Failed` names the pipeline stage that broke. A success can only ever be constructed from
//! a positive observation -- a re-read `currentPath`, a parsed narinfo, a health check that
//! actually ran and actually passed -- never from the absence of one.
//!
//! `Refused` deserves its own callout: refusing is this receiver's CORRECT behaviour when a
//! change would not survive activation on this machine (see `modules/default.nix`'s
//! `maxInplaceDeltaBytes`), not a defect in the run that produced it. Collapsing "refused"
//! into "failed" would make the safe, deliberate choice indistinguishable from something
//! having broken, and an operator (or an alert) that cannot tell those apart will eventually
//! stop trusting either signal. `exit_code` and `is_error` both preserve this distinction;
//! see their docs below.
//!
//! `Stage::HealthCheckUnavailable` vs `Stage::HealthCheckFailed` is the same principle
//! applied one level down, to a real incident this design is reacting to: a health-gate
//! command that could not even run (missing binary, bad interpreter path, no exec
//! permission) is a BROKEN PROBE, not evidence the machine is unhealthy. Folding the two
//! together turns a typo in a health-check path into an outage report, or worse, into a
//! rollback loop that reverts perfectly healthy work forever because the thing meant to
//! confirm it kept silently failing to run at all. Keeping them as distinct `Stage` values
//! (not just distinct words in a `detail` string) means nothing downstream can accidentally
//! treat them the same by pattern-matching on the wrong field.

use serde::{Deserialize, Serialize};

/// Every run of the receiver ends in exactly one of these. See the module doc for why there
/// is deliberately no sixth variant meaning "ran but did nothing."
///
/// `#[serde(tag = "outcome")]` gives an internally-tagged JSON shape
/// (`{"outcome":"converged","from":"...","to":"..."}`) so a consumer can dispatch on one
/// field without first guessing which other fields are present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum Outcome {
    /// Activated, health-gated, and confirmed by re-reading `currentPath` afterward -- see
    /// `activate.rs`. `from` and `to` are both store paths this receiver directly observed,
    /// never the value it merely intended to reach.
    Converged { from: String, to: String },

    /// `currentPath` already equalled the manifest's target before this run touched
    /// anything. `rev` is that observed path. Deliberately its own variant rather than a
    /// `Converged { from: rev, to: rev }` -- collapsing "nothing needed to change" into the
    /// same shape as "something changed" is exactly the ambiguity this type exists to
    /// remove (see the module doc).
    AlreadyCurrent { rev: String },

    /// The change was sized against this machine's own store (see `delta.rs`) and exceeded
    /// `nixdeploy.receiver.maxInplaceDeltaBytes`. Refusing is correct behaviour, not a
    /// failure -- see `exit_code` and `is_error`.
    Refused {
        reason: RefusedReason,
        /// Bytes of NEW store paths this machine would have had to fetch.
        bytes: u64,
        /// The ceiling that was exceeded. Always present on `Refused`: a ceiling of `null`
        /// (no limit) can never produce this variant in the first place.
        ceiling: u64,
    },

    /// A replacement of this machine with `image` was REQUESTED and the request was
    /// accepted -- not "this machine is now running that image", which is a claim this
    /// variant deliberately does not make and this binary could not honestly back.
    ///
    /// The reason is structural, not an implementation gap: the process that asks a
    /// provider to replace a machine is running ON the machine being replaced, so the
    /// moment the provider acts, that process stops existing. There are exactly three ways
    /// the reimage command can end for its caller -- it returns zero (the provider took the
    /// request), it returns non-zero (the provider rejected it, which is `Stage::Reimage`),
    /// or the caller is killed mid-call and returns nothing at all. Only the first two can
    /// produce any `Outcome` whatsoever, and neither of them has observed the replaced
    /// machine, because the replaced machine does not exist yet and will have no memory of
    /// the request when it does.
    ///
    /// So the confirming observation lives in a LATER run: the first `Converged` or
    /// `AlreadyCurrent` the replacement machine reports is the evidence the reimage
    /// actually landed. Until then the reimage is still owed, and `metrics.rs` keeps
    /// saying so (`nixdeploy_reimage_owed`) precisely so that "asked for a replacement and
    /// never got one" is visible instead of being read as a completed job.
    Reimaged { image: String },

    /// Something broke. `stage` says which pipeline step -- never left to a free-text
    /// `detail` alone, because a `detail` string is exactly the kind of field an
    /// alert-matching rule silently stops handling the day someone rewords it.
    Failed { stage: Stage, detail: String },
}

/// Why the receiver refused. Kept as an enum with one variant today, not a bare `String`,
/// so that a future second reason is an additive change to this type rather than a breaking
/// change to how every consumer parses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RefusedReason {
    /// The delta computed by `delta.rs` (bytes of new store paths this machine would have
    /// to fetch) exceeded the configured ceiling.
    DeltaExceedsCeiling,
}

/// Which stage of a run broke, for `Outcome::Failed`. Ordered roughly as a run proceeds
/// through it: config, then the manifest, then sizing the delta, then activation, then the
/// health gate (split into the two cases described in the module doc), then rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    /// The receiver's own on-disk config (manifest URL, public key, adapter commands,
    /// ceiling, health gate) could not be read or did not parse.
    Config,
    /// This machine's target could not be resolved: its own hostname could not be
    /// determined, the manifest could not be fetched, its signature did not verify, its
    /// schema version is not one this receiver understands, its body did not parse, or this
    /// hostname has no entry in it. See `manifest.rs`.
    Manifest,
    /// The size of the change could not be computed: the local store could not be queried,
    /// a `.narinfo` could not be fetched, or one failed to parse. See `delta.rs` -- a
    /// narinfo that fails to parse is always this, never treated as a zero-byte path.
    Delta,
    /// A refusal was routed to a reimage and the reimage could not be asked for: the
    /// configured command could not be spawned, it exited non-zero, or the manifest names
    /// no image for this machine to be replaced with.
    ///
    /// This is a `Failed` stage and not a second flavour of `Refused` on purpose. A plain
    /// refusal leaves the machine where it was with a route still open; this leaves it
    /// over its own ceiling with the ONLY remaining route broken -- the ratchet
    /// `docs/design.md` names, arrived at. The machine cannot activate in place (that is
    /// what produced the refusal) and cannot be replaced either, so nothing it does on its
    /// own schedule will change the answer. That deserves to be loud.
    Reimage,
    /// The `activate` adapter command ran (or could not even be spawned), but `currentPath`
    /// re-read afterward did not equal the target -- the machine did not actually become
    /// the closure it was given. See `activate.rs`.
    Activate,
    /// A health-gate command could not be run at all (missing binary, bad path, no exec
    /// permission). Distinct from `HealthCheckFailed` on purpose -- see the module doc. The
    /// receiver does NOT roll back on this: a probe that never ran says nothing about
    /// whether the new closure is healthy, and rolling back healthy work because a
    /// health-check script had a typo is the incident this variant exists to stop.
    HealthCheckUnavailable,
    /// A health-gate command ran and exited non-zero: the new closure is genuinely
    /// considered unhealthy. Rollback is attempted here (if the backend has one).
    HealthCheckFailed,
    /// The health gate failed AND the subsequent `rollback` adapter command could not
    /// recover the machine (or none is configured for this backend). The most urgent of the
    /// `Failed` stages: the machine may be left on an unhealthy closure with no automatic
    /// way back.
    Rollback,
}

/// Every value `Outcome::label` can return, in one place a caller can iterate.
///
/// `metrics.rs` needs this to emit a series for EVERY outcome on every run, not just the
/// one that happened -- see its module doc for the push-sink failure that requires it. A
/// missing entry here would silently mean an outcome no alert rule can ever match, so
/// `all_labels_are_the_wire_tags` below pins this list to the serialized form rather than
/// leaving the two to drift.
pub const OUTCOME_LABELS: [&str; 5] = [
    "converged",
    "alreadyCurrent",
    "refused",
    "reimaged",
    "failed",
];

impl Outcome {
    /// This outcome's name, identical to the `outcome` tag `serialize` puts on the wire.
    /// Exposed so a monitoring exposition does not have to serialize an `Outcome` to JSON
    /// and dig the tag back out of it -- and so both spellings of the same fact come from
    /// one `match`, which is what stops a renamed variant from meaning two different things
    /// to two consumers.
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Converged { .. } => "converged",
            Outcome::AlreadyCurrent { .. } => "alreadyCurrent",
            Outcome::Refused { .. } => "refused",
            Outcome::Reimaged { .. } => "reimaged",
            Outcome::Failed { .. } => "failed",
        }
    }

    /// A distinct exit code per top-level variant -- deliberately NOT the usual POSIX
    /// "0 means success, anything else means failure" scheme collapsed down to two buckets.
    /// The whole point of this type is that "did nothing," "succeeded," "correctly
    /// declined," and "was replaced" are different facts; collapsing them back together at
    /// the one place a shell script actually looks (the exit code) would silently undo
    /// everything the rest of this module does. Read the JSON on stdout (`serialize`) for
    /// the authoritative machine-readable answer; use `is_error` if you only want the
    /// coarse POSIX-style question answered honestly.
    ///
    /// A supervisor (e.g. a systemd unit) that wants "0 or nothing to alert on" behaviour
    /// for the non-`Failed` outcomes needs to list their codes in its own
    /// `SuccessExitStatus=` -- that configuration belongs to whoever wires this binary up,
    /// not to this library, which is exactly the "ceilings and policy are inputs, not
    /// opinions" rule the rest of this project follows. This repo's own systemd adapter
    /// (`modules/adapters/systemd-scheduling.nix`) is one such caller and lists `1 2 3`;
    /// without that, `AlreadyCurrent` -- the steady state of a converged fleet -- would put
    /// every receiver unit into `failed` on every tick.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::Converged { .. } => 0,
            Outcome::AlreadyCurrent { .. } => 1,
            Outcome::Reimaged { .. } => 2,
            Outcome::Refused { .. } => 3,
            Outcome::Failed { .. } => 4,
        }
    }

    /// The one question a caller that only wants POSIX-style success/failure should ask,
    /// instead of comparing `exit_code()` to zero -- because zero is not reserved for
    /// "nothing went wrong" here (see `exit_code`'s doc). Only `Failed` is ever an error:
    /// not `AlreadyCurrent` (there was nothing to do), not `Refused` (refusing was correct),
    /// not `Reimaged` (the machine was deliberately replaced).
    pub fn is_error(&self) -> bool {
        matches!(self, Outcome::Failed { .. })
    }

    /// The machine-readable form every run prints on stdout (see `main.rs`). Serialization
    /// of this type cannot fail -- every field is already an owned `String`, number, or one
    /// of this module's own enums -- so this returns a bare `String` rather than pushing a
    /// `Result` serde can never actually return an `Err` for onto every caller.
    pub fn serialize(&self) -> String {
        serde_json::to_string(self).expect("Outcome always serializes to JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_is_not_an_error_exit() {
        let refused = Outcome::Refused {
            reason: RefusedReason::DeltaExceedsCeiling,
            bytes: 10,
            ceiling: 5,
        };
        assert!(
            !refused.is_error(),
            "refusing is correct behaviour, not a failure"
        );
        let failed = Outcome::Failed {
            stage: Stage::Delta,
            detail: "unrelated".to_string(),
        };
        assert!(failed.is_error());
        assert_ne!(
            refused.exit_code(),
            failed.exit_code(),
            "a refusal and a failure must not share an exit code"
        );
    }

    #[test]
    fn probe_could_not_run_is_distinguishable_from_probe_failed() {
        let unavailable = Outcome::Failed {
            stage: Stage::HealthCheckUnavailable,
            detail: "exec: no such file or directory".to_string(),
        };
        let failed = Outcome::Failed {
            stage: Stage::HealthCheckFailed,
            detail: "exited 1".to_string(),
        };
        assert_ne!(
            unavailable, failed,
            "a broken probe and a genuinely unhealthy machine must not compare equal"
        );
        // The distinction must survive round-tripping through the wire format too --
        // encoded in the type, not just in an in-memory comparison a caller could bypass by
        // reading `detail` instead of `stage`.
        assert_ne!(unavailable.serialize(), failed.serialize());
        assert!(unavailable.serialize().contains("healthCheckUnavailable"));
        assert!(failed.serialize().contains("healthCheckFailed"));
    }

    #[test]
    fn exit_codes_are_distinct_per_variant() {
        let instances = [
            Outcome::Converged {
                from: "a".to_string(),
                to: "b".to_string(),
            },
            Outcome::AlreadyCurrent {
                rev: "a".to_string(),
            },
            Outcome::Reimaged {
                image: "img".to_string(),
            },
            Outcome::Refused {
                reason: RefusedReason::DeltaExceedsCeiling,
                bytes: 1,
                ceiling: 0,
            },
            Outcome::Failed {
                stage: Stage::Manifest,
                detail: "x".to_string(),
            },
        ];

        let mut codes: Vec<u8> = instances.iter().map(Outcome::exit_code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            instances.len(),
            "expected {} distinct exit codes, one per variant, got {:?}",
            instances.len(),
            instances.iter().map(Outcome::exit_code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_labels_are_the_wire_tags() {
        let one_of_each = [
            Outcome::Converged {
                from: "a".to_string(),
                to: "b".to_string(),
            },
            Outcome::AlreadyCurrent {
                rev: "a".to_string(),
            },
            Outcome::Refused {
                reason: RefusedReason::DeltaExceedsCeiling,
                bytes: 1,
                ceiling: 0,
            },
            Outcome::Reimaged {
                image: "img".to_string(),
            },
            Outcome::Failed {
                stage: Stage::Reimage,
                detail: "x".to_string(),
            },
        ];

        for outcome in &one_of_each {
            // The label must BE the serde tag, not merely resemble it: a metric labelled
            // `outcome="reimaged"` and a JSON line saying `"outcome":"replaced"` would send
            // an alert rule and a log grep looking for two different things.
            assert!(
                outcome
                    .serialize()
                    .contains(&format!("\"outcome\":\"{}\"", outcome.label())),
                "label {:?} is not the wire tag in {}",
                outcome.label(),
                outcome.serialize()
            );
            assert!(
                OUTCOME_LABELS.contains(&outcome.label()),
                "label {:?} is missing from OUTCOME_LABELS, so no metric series would ever \
                 exist for it",
                outcome.label()
            );
        }
        assert_eq!(
            OUTCOME_LABELS.len(),
            one_of_each.len(),
            "OUTCOME_LABELS must list every variant and nothing else"
        );
    }

    #[test]
    fn serialized_shape_is_internally_tagged_and_camel_case() {
        let converged = Outcome::Converged {
            from: "/nix/store/aaa-old".to_string(),
            to: "/nix/store/bbb-new".to_string(),
        };
        let json = converged.serialize();
        assert!(json.contains("\"outcome\":\"converged\""));
        assert!(json.contains("\"from\":\"/nix/store/aaa-old\""));
        assert!(json.contains("\"to\":\"/nix/store/bbb-new\""));
    }
}
