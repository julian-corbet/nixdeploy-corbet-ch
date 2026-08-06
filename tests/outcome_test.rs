//! Integration tests against `outcome::Outcome` as a real external consumer would see it --
//! through the crate's public API only (`nixdeploy::{Outcome, RefusedReason, Stage}`), not
//! through `outcome.rs`'s own in-file unit tests. See `Cargo.toml`'s `[lib]` comment for why
//! `outcome.rs` is this crate's library target.
//!
//! These three tests are the three claims `outcome.rs`'s design is supposed to make true;
//! each one is written so that reverting the corresponding design decision (making Refused
//! an error exit, conflating the two probe-failure kinds, or collapsing exit codes back to
//! 0/1) would fail it.

use nixdeploy::{Outcome, RefusedReason, Stage};

#[test]
fn refused_is_not_a_failure_exit() {
    let refused = Outcome::Refused {
        reason: RefusedReason::DeltaExceedsCeiling,
        bytes: 900_000_000,
        ceiling: 500_000_000,
    };

    // The literal claim from README.md's "Outcomes are typed" section: refusing is safe,
    // deliberate, and NOT a failure.
    assert!(
        !refused.is_error(),
        "Refused must not read as an error to a caller checking is_error()"
    );

    let failed = Outcome::Failed {
        stage: Stage::Delta,
        detail: "narinfo fetch failed".to_string(),
    };
    assert!(failed.is_error());

    // Not just "is_error() differs" -- the raw exit code must differ too, since that is
    // the signal a plain shell `if nixdeploy; then` actually observes.
    assert_ne!(
        refused.exit_code(),
        failed.exit_code(),
        "a refusal and a genuine failure must not share a process exit code"
    );
}

#[test]
fn probe_could_not_run_is_a_different_fact_from_probe_failed() {
    let could_not_run = Outcome::Failed {
        stage: Stage::HealthCheckUnavailable,
        detail: "exec: /nix/store/xxx-check/bin/check: no such file or directory".to_string(),
    };
    let ran_and_failed = Outcome::Failed {
        stage: Stage::HealthCheckFailed,
        detail: "exited 1".to_string(),
    };

    // A missing binary and a genuinely unhealthy machine must be structurally
    // distinguishable, not just differently worded -- otherwise a rollback policy that
    // pattern-matches on `stage` (the whole point of carrying a typed `Stage`, not a bare
    // string) cannot tell them apart.
    assert_ne!(could_not_run, ran_and_failed);
    match could_not_run {
        Outcome::Failed { stage, .. } => assert_eq!(stage, Stage::HealthCheckUnavailable),
        _ => panic!("expected Failed"),
    }
    match ran_and_failed {
        Outcome::Failed { stage, .. } => assert_eq!(stage, Stage::HealthCheckFailed),
        _ => panic!("expected Failed"),
    }

    // And the distinction has to reach the wire, not just live in an enum comparison a
    // caller could bypass by only ever reading `detail`.
    let could_not_run_json = could_not_run.serialize();
    let ran_and_failed_json = ran_and_failed.serialize();
    assert!(could_not_run_json.contains("\"stage\":\"healthCheckUnavailable\""));
    assert!(ran_and_failed_json.contains("\"stage\":\"healthCheckFailed\""));
}

#[test]
fn every_variant_has_a_distinct_exit_code() {
    let one_of_each = [
        Outcome::Converged {
            from: "/nix/store/aaa-old".to_string(),
            to: "/nix/store/bbb-new".to_string(),
        },
        Outcome::AlreadyCurrent {
            rev: "/nix/store/aaa-old".to_string(),
        },
        Outcome::Reimaged {
            role: nixdeploy::manifest::BootRole::Primary,
            artifact: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-primary".to_string(),
            image: "example-image-2026-08".to_string(),
        },
        Outcome::Refused {
            reason: RefusedReason::DeltaExceedsCeiling,
            bytes: 2,
            ceiling: 1,
        },
        Outcome::Failed {
            stage: Stage::Manifest,
            detail: "signature did not verify".to_string(),
        },
    ];

    let codes: std::collections::HashSet<u8> = one_of_each.iter().map(Outcome::exit_code).collect();
    assert_eq!(
        codes.len(),
        one_of_each.len(),
        "expected {} distinct exit codes (one per Outcome variant), got {} distinct values",
        one_of_each.len(),
        codes.len()
    );

    // Only Failed should ever be considered an error by is_error() -- the other four are
    // all legitimate, distinguishable non-error outcomes.
    let error_count = one_of_each.iter().filter(|o| o.is_error()).count();
    assert_eq!(
        error_count, 1,
        "only Failed should report is_error() == true"
    );
}
