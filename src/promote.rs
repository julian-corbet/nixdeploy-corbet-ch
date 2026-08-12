//! Trusted release-service command surface over [`crate::release`].
//!
//! Builders hand this command an unsigned candidate host map. The signing key stays here,
//! at the serialized promotion boundary. Every well-formed request ends in one durable local
//! result (`promoted`, `unchanged`, `superseded`, or `rejected`); infrastructure failures do
//! not write a result and are safe to retry. That distinction is what lets a queue release
//! its GC roots instead of replaying old descriptors forever.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::atomicfile::write_atomic;
use crate::manifest::parse_signing_key;
use crate::release::{PromotionOutcome, PromotionRequest, ReleaseHostEntry, ReleaseStore};

#[derive(Debug, Clone)]
pub struct PromoteArgs {
    pub targets_file: PathBuf,
    pub origin: PathBuf,
    pub expected_base: Option<String>,
    pub hosts: BTreeSet<String>,
    pub planes: BTreeSet<String>,
    pub signing_key_file: PathBuf,
    pub published_at: Option<String>,
    pub request_id: String,
    pub result_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RecoverArgs {
    pub origin: PathBuf,
    pub signing_key_file: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Recovered {
    pub recovered_deployment_set_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionResult {
    pub version: u32,
    pub request_id: String,
    pub outcome: PromotionOutcome,
}

impl PromotionResult {
    pub fn serialize(&self) -> String {
        serde_json::to_string(self).expect("PromotionResult always serializes")
    }
}

#[derive(Debug)]
pub enum PromoteError {
    Usage(String),
    Read(PathBuf, String),
    Parse(PathBuf, String),
    Key(PathBuf, String),
    Promote(String),
    Write(PathBuf, String),
}

impl fmt::Display for PromoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromoteError::Usage(error) | PromoteError::Promote(error) => f.write_str(error),
            PromoteError::Read(path, error) => write!(f, "reading {}: {}", path.display(), error),
            PromoteError::Parse(path, error) => {
                write!(f, "parsing {}: {}", path.display(), error)
            }
            PromoteError::Key(path, error) => {
                write!(f, "signing key {}: {}", path.display(), error)
            }
            PromoteError::Write(path, error) => {
                write!(f, "writing terminal result {}: {}", path.display(), error)
            }
        }
    }
}

impl std::error::Error for PromoteError {}

pub fn promote(args: &PromoteArgs, now_unix: u64) -> Result<PromotionResult, PromoteError> {
    let targets_text = fs::read_to_string(&args.targets_file)
        .map_err(|e| PromoteError::Read(args.targets_file.clone(), e.to_string()))?;
    let candidates: BTreeMap<String, ReleaseHostEntry> = serde_json::from_str(&targets_text)
        .map_err(|e| PromoteError::Parse(args.targets_file.clone(), e.to_string()))?;

    let key_text = fs::read_to_string(&args.signing_key_file)
        .map_err(|e| PromoteError::Read(args.signing_key_file.clone(), e.to_string()))?;
    let (key_name, key) = parse_signing_key(&key_text)
        .map_err(|e| PromoteError::Key(args.signing_key_file.clone(), e))?;

    let request = PromotionRequest {
        candidates,
        hosts: args.hosts.clone(),
        planes: args.planes.clone(),
        expected_base: args.expected_base.clone(),
        published_at: args
            .published_at
            .clone()
            .unwrap_or_else(|| iso8601_utc(now_unix)),
    };
    let outcome = ReleaseStore::new(&args.origin)
        .promote(&request, &key_name, &key)
        .map_err(|e| PromoteError::Promote(e.to_string()))?;
    let result = PromotionResult {
        version: 1,
        request_id: args.request_id.clone(),
        outcome,
    };
    let mut bytes = serde_json::to_vec_pretty(&result)
        .expect("PromotionResult contains only serializable fields");
    bytes.push(b'\n');
    write_atomic(&args.result_file, &bytes, 0o644)
        .map_err(|e| PromoteError::Write(args.result_file.clone(), e))?;
    Ok(result)
}

/// Repairs only the mutable stable channel from the newest verified immutable promotion
/// record. It never evaluates a candidate and never replays a historical queue request.
pub fn recover(args: &RecoverArgs) -> Result<Recovered, PromoteError> {
    let key_text = fs::read_to_string(&args.signing_key_file)
        .map_err(|e| PromoteError::Read(args.signing_key_file.clone(), e.to_string()))?;
    let (_, key) = parse_signing_key(&key_text)
        .map_err(|e| PromoteError::Key(args.signing_key_file.clone(), e))?;
    let recovered_deployment_set_id = ReleaseStore::new(&args.origin)
        .recover(&key)
        .map_err(|e| PromoteError::Promote(e.to_string()))?;
    Ok(Recovered {
        recovered_deployment_set_id,
    })
}

pub fn parse_recover_args(args: &[String]) -> Result<RecoverArgs, PromoteError> {
    let mut origin = None;
    let mut signing_key_file = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (args[i].as_str(), None),
        };
        match flag {
            "--origin" => origin = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?)),
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?))
            }
            "--signing-key" | "--key" => {
                return Err(PromoteError::Usage(
                    "the signing key is accepted only through --signing-key-file".to_string(),
                ));
            }
            other => {
                return Err(PromoteError::Usage(format!(
                    "unknown flag {:?} -- see `nixdeploy recover` usage",
                    other
                )));
            }
        }
        i += 1;
    }
    Ok(RecoverArgs {
        origin: origin.ok_or_else(|| PromoteError::Usage("--origin is required".to_string()))?,
        signing_key_file: signing_key_file
            .ok_or_else(|| PromoteError::Usage("--signing-key-file is required".to_string()))?,
    })
}

pub fn parse_args(args: &[String]) -> Result<PromoteArgs, PromoteError> {
    let mut targets_file = None;
    let mut origin = None;
    let mut expected_base: Option<Option<String>> = None;
    let mut hosts = BTreeSet::new();
    let mut planes = BTreeSet::new();
    let mut signing_key_file = None;
    let mut published_at = None;
    let mut request_id = None;
    let mut result_file = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (args[i].as_str(), None),
        };
        match flag {
            "--targets" => {
                targets_file = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?))
            }
            "--origin" => origin = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?)),
            "--expected-base" => {
                let value = value_of(args, &mut i, flag, inline)?;
                expected_base = Some(if value == "none" { None } else { Some(value) });
            }
            "--host" => {
                hosts.insert(value_of(args, &mut i, flag, inline)?);
            }
            "--plane" => {
                let value = value_of(args, &mut i, flag, inline)?;
                if !["nixos", "system-manager", "home-manager", "nix-darwin"]
                    .contains(&value.as_str())
                {
                    return Err(PromoteError::Usage(format!(
                        "--plane {:?} is not a known activation plane",
                        value
                    )));
                }
                planes.insert(value);
            }
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?))
            }
            "--published-at" => published_at = Some(value_of(args, &mut i, flag, inline)?),
            "--request-id" => request_id = Some(value_of(args, &mut i, flag, inline)?),
            "--result" => result_file = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?)),
            "--signing-key" | "--key" => {
                return Err(PromoteError::Usage(
                    "the signing key is accepted only through --signing-key-file".to_string(),
                ));
            }
            other => {
                return Err(PromoteError::Usage(format!(
                    "unknown flag {:?} -- see `nixdeploy promote` usage",
                    other
                )));
            }
        }
        i += 1;
    }

    let request_id =
        request_id.ok_or_else(|| PromoteError::Usage("--request-id is required".to_string()))?;
    if request_id.trim().is_empty() {
        return Err(PromoteError::Usage(
            "--request-id must not be empty".to_string(),
        ));
    }
    Ok(PromoteArgs {
        targets_file: targets_file
            .ok_or_else(|| PromoteError::Usage("--targets is required".to_string()))?,
        origin: origin.ok_or_else(|| PromoteError::Usage("--origin is required".to_string()))?,
        expected_base: expected_base
            .ok_or_else(|| PromoteError::Usage("--expected-base is required".to_string()))?,
        hosts,
        planes,
        signing_key_file: signing_key_file
            .ok_or_else(|| PromoteError::Usage("--signing-key-file is required".to_string()))?,
        published_at,
        request_id,
        result_file: result_file
            .ok_or_else(|| PromoteError::Usage("--result is required".to_string()))?,
    })
}

fn value_of(
    args: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<String>,
) -> Result<String, PromoteError> {
    match inline {
        Some(value) => Ok(value),
        None => {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| PromoteError::Usage(format!("{} needs a value", flag)))
        }
    }
}

fn iso8601_utc(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let seconds = unix_seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    if month <= 2 {
        year += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

pub const USAGE: &str = "\
nixdeploy promote --targets FILE --origin DIR --expected-base ID|none
                  --signing-key-file FILE --request-id ID --result FILE
                  [--published-at TS] [--host HOST...] [--plane PLANE...]

Atomically compose and promote a signed schema-v4 deployment set. The targets file is an
unsigned candidate host map with per-artifact provenance. Every terminal request writes FILE;
infrastructure errors write no terminal result and are safe to retry.";

pub const RECOVER_USAGE: &str = "\
nixdeploy recover --origin DIR --signing-key-file FILE

Verify the immutable promotion journal and restore the exact stable-channel bytes selected by
its newest record. Never replays a queued candidate or invokes an older publisher.";
