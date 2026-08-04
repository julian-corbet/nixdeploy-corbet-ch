//! The `nixdeploy` binary: a dispatcher over the two halves of one delivery mechanism.
//!
//! ```text
//! nixdeploy receive [-config PATH]   the machine deciding what it can become
//! nixdeploy publish  --hosts ...     the builder saying what every machine should be
//! ```
//!
//! One binary, two subcommands, rather than two crates -- see `lib.rs` for why. There is
//! deliberately no default subcommand: a bare `nixdeploy` that quietly meant `receive` would
//! be a footgun on the publisher, which is a machine that must never activate anything.
//!
//! Everything below is argument handling and printing. The pipelines themselves live in
//! `receive.rs` and `publish.rs`, where they are reachable from tests without a process.

use std::env;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use nixdeploy::publish;
use nixdeploy::receive;

/// `EX_USAGE` from `sysexits.h`. Deliberately outside the 0-4 range `Outcome::exit_code`
/// uses: "you called this binary wrong" must not be mistakable for "the machine refused" or
/// any other outcome a run can produce, since those are the codes a supervisor is configured
/// to interpret.
const EXIT_USAGE: u8 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((subcommand, rest)) = args.split_first() else {
        eprintln!("{}", USAGE);
        return ExitCode::from(EXIT_USAGE);
    };

    match subcommand.as_str() {
        "receive" => receive_main(rest),
        "publish" => publish_main(rest),
        "-h" | "--help" | "help" => {
            println!("{}", USAGE);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("nixdeploy: unknown subcommand {:?}\n\n{}", other, USAGE);
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn receive_main(args: &[String]) -> ExitCode {
    let config_path = receive::parse_args(args);
    let outcome = receive::run(&config_path);
    println!("{}", outcome.serialize());
    if outcome.is_error() {
        // Only a genuine failure is noisy on stderr -- AlreadyCurrent, Refused and
        // Reimaged are all legitimate outcomes an operator watching logs should not have
        // to triage, so they never print anything beyond the JSON line above.
        eprintln!("nixdeploy: run failed, see the JSON on stdout for which stage");
    }
    ExitCode::from(outcome.exit_code())
}

fn publish_main(args: &[String]) -> ExitCode {
    let parsed = match publish::parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("nixdeploy publish: {}\n\n{}", e, publish::USAGE);
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match publish::publish(&parsed, now) {
        Ok(published) => {
            for warning in &published.warnings {
                eprintln!("nixdeploy publish: {}", warning);
            }
            // The same convention the receiver follows: one machine-readable line on stdout
            // naming what actually happened, so a caller never has to infer it from an exit
            // code alone.
            println!("{}", published.serialize());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("nixdeploy publish: {}", e);
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
nixdeploy -- deliver a prebuilt closure to a machine that did not build it.

  nixdeploy receive [-config PATH]
      Read the manifest, size the change against THIS machine's own store, and
      activate, refuse, or route the refusal to a reimage. Prints one JSON outcome.
      Config defaults to /etc/nixdeploy/config.json.

  nixdeploy publish --hosts FILE --revision REV --signing-key-file FILE --out FILE
      Render the manifest, sign it, write it and its detached signature.
      Builds nothing and uploads nothing. `nixdeploy publish` with no arguments
      prints its own flags.";
