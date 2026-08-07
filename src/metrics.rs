//! What happened, in a form a monitoring system can read -- the other half of "you must know
//! afterwards whether it did" (`README.md`). The JSON `Outcome` on stdout answers that for
//! whoever is looking at one machine's journal; this module answers it for whoever is not
//! looking at anything, which is the case that actually matters.
//!
//! # The failure this exists for is silence, not a bad value
//!
//! A receiver that refuses, fails, or never runs at all produces no alert on its own. The
//! machine that quietly stopped converging three weeks ago looks exactly like the machine
//! that has had nothing to do for three weeks, and the only difference between them is that
//! one of them stopped reporting. So the design constraint here is not "emit a metric when
//! something goes wrong" -- it is that a metric must ALWAYS exist and must ALWAYS carry a
//! fresh timestamp, so that a monitoring system can alert on the ABSENCE of a recent run
//! (`time() - nixdeploy_run_timestamp_seconds > <interval * 3>`) without needing this binary
//! to have been alive to tell it. An error-only metric cannot do that: the run that never
//! happened emits no error either.
//!
//! # Why every outcome gets its own series
//!
//! `nixdeploy_run_outcome` is emitted once per outcome NAME (see `outcome::OUTCOME_LABELS`),
//! with a 1 on the one that happened and an explicit 0 on the other four -- rather than a
//! single series whose label value changes. Under the textfile sink the two are equivalent,
//! because the whole file is replaced. Under a push sink they are not: a push gateway keeps
//! whatever it was last told until something overwrites that exact series, and a series
//! nobody mentions again is a series that keeps its old value forever. A run that converges
//! after a failure would then leave `outcome="failed" 1` standing next to the new
//! `outcome="converged" 1`, and every alert built on the first one fires forever on a fleet
//! that is fine. Writing the zeroes is what retires the previous answer.
//!
//! For the same reason the label set here is FIXED and small. Nothing carries a store path, a
//! hostname, a failure stage or any other value that varies per run: on a sink that never
//! expires a series, each distinct label value is a series that lives forever. Those details
//! belong in the JSON outcome, which is a log line and is allowed to be unique every time.
//!
//! # A sink is never allowed to change what happened
//!
//! `emit` returns the sinks' errors instead of a `Result` the caller might use to fail the
//! run. A machine that converged and then could not write a metrics file has converged;
//! reporting that as a failed deploy would make the monitoring system the most fragile
//! dependency in a system built specifically to have no single point of failure.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::atomicfile::write_atomic;
use crate::outcome::{Outcome, OUTCOME_LABELS};

/// Prometheus' text exposition content type. Sent on the push so a receiving endpoint that
/// content-negotiates (a push gateway, an OTLP-ish shim) parses the body instead of storing
/// it as an opaque blob.
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// The push must not be able to hold a run open. A receiver runs on a timer; a metrics
/// endpoint that accepts a connection and then never answers would otherwise leave the
/// process resident until the next tick starts a second one alongside it.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Where run outcomes are reported, if anywhere. Both sinks are independent and both are off
/// unless configured -- this repo ships mechanism, and where an estate's metrics go is
/// policy it supplies (`README.md`: "Not a monitoring stack").
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsConfig {
    /// A Prometheus textfile-collector file, written atomically (see `write_textfile`).
    #[serde(default)]
    pub textfile: Option<PathBuf>,
    /// A URL the same exposition text is POSTed to. For machines a scraper cannot reach --
    /// which is most of the machines this repo exists for, since a receiver's whole premise
    /// is that reachability is not something it can depend on.
    #[serde(default)]
    pub push_url: Option<String>,
}

impl MetricsConfig {
    /// Whether any sink is configured at all. Used to skip rendering entirely, so the common
    /// "no metrics configured" case costs nothing beyond this check.
    pub fn is_enabled(&self) -> bool {
        self.textfile.is_some() || self.push_url.is_some()
    }
}

/// One run's reportable facts. Assembled by `receive.rs`, which is the only thing that knows
/// them: the `Outcome` alone cannot carry the delta of a run that CONVERGED (it names two
/// store paths, not a byte count), and a monitoring system that only learns the delta when a
/// machine refuses cannot see the machine that is creeping toward its ceiling.
#[derive(Debug, Clone)]
pub struct RunReport<'a> {
    pub outcome: &'a Outcome,
    /// Bytes of new store paths measured against this machine's own store, where a
    /// measurement was actually taken. `None` for a run that ended before `delta.rs` ran --
    /// deliberately not zero, because zero is a real and different measurement meaning
    /// "everything the target needs is already here".
    pub delta_bytes: Option<u64>,
    /// The ceiling that was in force. `None` means no ceiling is configured, which is a
    /// deliberate setting (see `maxInplaceDeltaBytes`) and not a missing value -- again not
    /// zero, which would read as "refuse everything", the exact inversion.
    pub ceiling: Option<u64>,
    /// Whether this machine is known to need replacing: its delta came back over its
    /// ceiling, so it cannot make progress in place.
    ///
    /// This stays 1 after a reimage has been ASKED for, not just while it is unrouted,
    /// because asking is all this binary can observe (see `Outcome::Reimaged`). It returns
    /// to 0 only when some later run actually converges or finds itself already current --
    /// which makes `nixdeploy_reimage_owed == 1` for longer than one interval the single
    /// alert that catches both "no reimage command was configured" and "one was, and the
    /// machine never came back".
    pub reimage_owed: bool,
    /// UNIX seconds. Passed in rather than read here so a test can pin it, and so every
    /// metric in one exposition carries the same instant.
    pub timestamp: u64,
}

/// A sink that did not work, named so `receive.rs` can print it without having to know how
/// many sinks there are or which one failed.
#[derive(Debug)]
pub struct SinkError {
    pub sink: &'static str,
    pub detail: String,
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sink: {}", self.sink, self.detail)
    }
}

/// Renders the exposition text for one run. Pure: no I/O, so the exact bytes both sinks
/// carry are testable without either sink existing.
pub fn render(report: &RunReport<'_>) -> String {
    let mut out = String::new();

    // Always first and always present. This is the metric a staleness alert watches, so it
    // must not be conditional on anything -- including on the run having gone well.
    out.push_str(
        "# HELP nixdeploy_run_timestamp_seconds UNIX time of the most recent nixdeploy run on \
         this machine. Always emitted, whatever the outcome: a machine that stops reporting \
         this is the failure mode an outcome-only metric cannot express.\n",
    );
    out.push_str("# TYPE nixdeploy_run_timestamp_seconds gauge\n");
    let _ = writeln!(out, "nixdeploy_run_timestamp_seconds {}", report.timestamp);

    out.push_str(
        "# HELP nixdeploy_run_outcome The outcome of the most recent run: 1 on the outcome \
         that happened, 0 on every other, so a previous run's answer is retired rather than \
         left standing on a sink that never expires a series.\n",
    );
    out.push_str("# TYPE nixdeploy_run_outcome gauge\n");
    for label in OUTCOME_LABELS {
        let value = u8::from(label == report.outcome.label());
        let _ = writeln!(
            out,
            "nixdeploy_run_outcome{{outcome=\"{}\"}} {}",
            label, value
        );
    }

    if let Some(bytes) = report.delta_bytes {
        out.push_str(
            "# HELP nixdeploy_delta_bytes Bytes of new store paths this run measured against \
             this machine's own store. Absent when the run ended before measuring; 0 means \
             measured and nothing to fetch.\n",
        );
        out.push_str("# TYPE nixdeploy_delta_bytes gauge\n");
        let _ = writeln!(out, "nixdeploy_delta_bytes {}", bytes);
    }

    if let Some(ceiling) = report.ceiling {
        out.push_str(
            "# HELP nixdeploy_delta_ceiling_bytes The in-place ceiling in force on this \
             machine. Absent when no ceiling is configured, which is a deliberate setting and \
             not an unknown value.\n",
        );
        out.push_str("# TYPE nixdeploy_delta_ceiling_bytes gauge\n");
        let _ = writeln!(out, "nixdeploy_delta_ceiling_bytes {}", ceiling);
    }

    out.push_str(
        "# HELP nixdeploy_reimage_owed 1 when this machine's delta exceeded its ceiling and \
         it therefore cannot converge in place. Stays 1 after a reimage has been requested, \
         because the request is all the receiver can observe; only a later run that converges \
         clears it.\n",
    );
    out.push_str("# TYPE nixdeploy_reimage_owed gauge\n");
    let _ = writeln!(
        out,
        "nixdeploy_reimage_owed {}",
        u8::from(report.reimage_owed)
    );

    out
}

/// Renders `report` and pushes it at every configured sink, returning whatever failed.
/// Never returns a `Result`: see the module doc -- a sink cannot change what happened.
pub fn emit(cfg: &MetricsConfig, report: &RunReport<'_>) -> Vec<SinkError> {
    if !cfg.is_enabled() {
        return Vec::new();
    }
    let text = render(report);
    let mut errors = Vec::new();

    if let Some(path) = &cfg.textfile {
        // Atomic because a textfile collector scrapes on its own schedule with no
        // coordination with this process, and a truncated exposition is not a missing
        // metric -- it is a parse error that makes the collector discard the whole file,
        // including the staleness timestamp that was meant to be the last thing to fail.
        // Mode 0644 because the collector usually runs as a different user than the
        // receiver (which needs privilege to activate), and an owner-only file is one the
        // scraper silently cannot read.
        if let Err(detail) = write_atomic(path, text.as_bytes(), 0o644) {
            errors.push(SinkError {
                sink: "textfile",
                detail,
            });
        }
    }

    if let Some(url) = &cfg.push_url {
        if let Err(detail) = push(url, &text) {
            errors.push(SinkError {
                sink: "push",
                detail,
            });
        }
    }

    errors
}

fn push(url: &str, text: &str) -> Result<(), String> {
    let response = ureq::post(url)
        .timeout(PUSH_TIMEOUT)
        .set("Content-Type", EXPOSITION_CONTENT_TYPE)
        .send_string(text)
        .map_err(|e| format!("POST {}: {}", url, e))?;
    if response.status() >= 300 {
        return Err(format!(
            "POST {} answered {} {}",
            url,
            response.status(),
            response.status_text()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{RefusedReason, Stage};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nixdeploy-metrics-{}-{}", tag, process::id()));
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    fn report<'a>(outcome: &'a Outcome) -> RunReport<'a> {
        RunReport {
            outcome,
            delta_bytes: None,
            ceiling: None,
            reimage_owed: false,
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn every_outcome_emits_a_fresh_timestamp_and_a_full_outcome_enumeration() {
        // The staleness claim, checked against EVERY outcome rather than the happy one: a
        // machine that only reports when it succeeds is a machine whose failures look
        // identical to its silence.
        let outcomes = [
            Outcome::Converged {
                from: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
                to: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-new".to_string(),
            },
            Outcome::AlreadyCurrent {
                rev: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
            },
            Outcome::Refused {
                reason: RefusedReason::DeltaExceedsCeiling,
                bytes: 900,
                ceiling: 500,
            },
            Outcome::Reimaged {
                image: "image-2026-08".to_string(),
            },
            Outcome::Failed {
                stage: Stage::Manifest,
                detail: "signature did not verify".to_string(),
            },
        ];

        for outcome in &outcomes {
            let text = render(&report(outcome));
            assert!(
                text.contains("nixdeploy_run_timestamp_seconds 1700000000\n"),
                "no timestamp for {:?}:\n{}",
                outcome,
                text
            );
            assert!(
                text.contains("nixdeploy_reimage_owed "),
                "no reimage_owed for {:?}",
                outcome
            );

            let ones: Vec<&str> = OUTCOME_LABELS
                .iter()
                .copied()
                .filter(|l| {
                    text.contains(&format!("nixdeploy_run_outcome{{outcome=\"{}\"}} 1\n", l))
                })
                .collect();
            assert_eq!(
                ones,
                vec![outcome.label()],
                "exactly one outcome series must be 1, for {:?}:\n{}",
                outcome,
                text
            );
            for label in OUTCOME_LABELS {
                assert!(
                    text.contains(&format!("nixdeploy_run_outcome{{outcome=\"{}\"}} ", label)),
                    "outcome {:?} has no series at all, so nothing retires its previous \
                     value on a push sink:\n{}",
                    label,
                    text
                );
            }
        }
    }

    #[test]
    fn a_measured_zero_delta_is_reported_and_an_unmeasured_one_is_not() {
        // Zero bytes is a real measurement ("everything is already here"). Omitting a metric
        // and reporting 0 must not be the same thing, or a run that never got as far as
        // measuring would read as a machine that is perfectly up to date.
        let outcome = Outcome::AlreadyCurrent {
            rev: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
        };

        let measured = render(&RunReport {
            delta_bytes: Some(0),
            ..report(&outcome)
        });
        assert!(
            measured.contains("nixdeploy_delta_bytes 0\n"),
            "{}",
            measured
        );

        let unmeasured = render(&report(&outcome));
        assert!(
            !unmeasured.contains("nixdeploy_delta_bytes"),
            "an unmeasured delta must not be reported as any number:\n{}",
            unmeasured
        );
    }

    #[test]
    fn ceiling_and_reimage_owed_are_reported_from_the_run_not_the_outcome() {
        let outcome = Outcome::Refused {
            reason: RefusedReason::DeltaExceedsCeiling,
            bytes: 900,
            ceiling: 500,
        };
        let text = render(&RunReport {
            delta_bytes: Some(900),
            ceiling: Some(500),
            reimage_owed: true,
            ..report(&outcome)
        });
        assert!(text.contains("nixdeploy_delta_bytes 900\n"), "{}", text);
        assert!(
            text.contains("nixdeploy_delta_ceiling_bytes 500\n"),
            "{}",
            text
        );
        assert!(text.contains("nixdeploy_reimage_owed 1\n"), "{}", text);

        // No ceiling configured is not a ceiling of zero.
        let no_ceiling = render(&report(&outcome));
        assert!(
            !no_ceiling.contains("nixdeploy_delta_ceiling_bytes"),
            "{}",
            no_ceiling
        );
    }

    #[test]
    fn every_metric_line_is_a_complete_line() {
        // A textfile collector drops the WHOLE file on a parse error, so a missing final
        // newline costs every metric in it, including the staleness timestamp.
        let outcome = Outcome::Reimaged {
            image: "image-2026-08".to_string(),
        };
        let text = render(&RunReport {
            delta_bytes: Some(1),
            ceiling: Some(2),
            reimage_owed: true,
            ..report(&outcome)
        });
        assert!(text.ends_with('\n'), "exposition must end with a newline");
        for line in text.lines() {
            assert!(!line.is_empty(), "blank line in exposition:\n{}", text);
        }
    }

    #[test]
    fn textfile_is_replaced_by_rename_and_leaves_no_temp_behind() {
        let dir = tmpdir("atomic");
        let path = dir.join("nixdeploy.prom");
        fs::write(&path, "stale content that must be gone\n").expect("seed");

        let outcome = Outcome::Converged {
            from: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
            to: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-new".to_string(),
        };
        let cfg = MetricsConfig {
            textfile: Some(path.clone()),
            push_url: None,
        };
        let errors = emit(&cfg, &report(&outcome));
        assert!(errors.is_empty(), "{:?}", errors);

        let written = fs::read_to_string(&path).expect("read back");
        assert!(!written.contains("stale content"));
        assert!(written.contains("nixdeploy_run_outcome{outcome=\"converged\"} 1\n"));
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o644,
            "a collector running as another user must be able to read it"
        );

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "nixdeploy.prom")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must be renamed away, not accumulated: {:?}",
            leftovers
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unconfigured_sinks_do_nothing_at_all() {
        let outcome = Outcome::AlreadyCurrent {
            rev: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
        };
        assert!(emit(&MetricsConfig::default(), &report(&outcome)).is_empty());
    }

    #[test]
    fn both_sinks_report_their_own_failure_and_neither_hides_the_other() {
        let outcome = Outcome::Converged {
            from: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-old".to_string(),
            to: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-new".to_string(),
        };
        let cfg = MetricsConfig {
            textfile: Some(PathBuf::from(
                "/nonexistent-nixdeploy-dir/collector/nixdeploy.prom",
            )),
            // Port 1 refuses immediately on every OS this runs on: a real connection
            // failure with no DNS lookup and no waiting.
            push_url: Some("http://127.0.0.1:1/metrics".to_string()),
        };

        let errors = emit(&cfg, &report(&outcome));
        let sinks: Vec<&str> = errors.iter().map(|e| e.sink).collect();
        assert_eq!(
            sinks,
            vec!["textfile", "push"],
            "one broken sink must not stop the other from being tried: {:?}",
            errors
        );
    }

    #[test]
    fn config_parses_from_module_shaped_json() {
        let cfg: MetricsConfig = serde_json::from_str(
            r#"{"textfile":"/var/lib/collector/nixdeploy.prom","pushUrl":"https://example.org/metrics/job/nixdeploy"}"#,
        )
        .expect("parse");
        assert_eq!(
            cfg.textfile,
            Some(PathBuf::from("/var/lib/collector/nixdeploy.prom"))
        );
        assert!(cfg.is_enabled());

        let empty: MetricsConfig = serde_json::from_str("{}").expect("parse empty");
        assert!(
            !empty.is_enabled(),
            "metrics must be off unless configured, not on by default"
        );
    }
}
