//! Read-only verification surface for consumers of a signed release.
//!
//! Cache retention, audit tools and operators must not reimplement the release envelope or
//! scrape its base64 payload with `jq`. This command verifies the exact signed bytes first and
//! then returns the complete set of Nix store roots named by the trusted deployment set.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::release::{verify_release, ReleaseBootSpec, ReleaseDocument};

#[derive(Debug, Clone)]
pub struct VerifyReleaseArgs {
    pub release_file: PathBuf,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInventory {
    pub version: u32,
    pub deployment_set_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_deployment_set_id: Option<String>,
    pub published_at: String,
    pub targets: Vec<String>,
}

impl ReleaseInventory {
    pub fn serialize(&self) -> String {
        serde_json::to_string(self).expect("ReleaseInventory always serializes")
    }
}

#[derive(Debug)]
pub enum VerifyReleaseError {
    Usage(String),
    Read(PathBuf, String),
    Trust(PathBuf, String),
}

impl fmt::Display for VerifyReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyReleaseError::Usage(error) => f.write_str(error),
            VerifyReleaseError::Read(path, error) => {
                write!(f, "reading {}: {}", path.display(), error)
            }
            VerifyReleaseError::Trust(path, error) => {
                write!(f, "refusing untrusted {}: {}", path.display(), error)
            }
        }
    }
}

impl std::error::Error for VerifyReleaseError {}

pub fn verify(args: &VerifyReleaseArgs) -> Result<ReleaseInventory, VerifyReleaseError> {
    let bytes = fs::read(&args.release_file)
        .map_err(|e| VerifyReleaseError::Read(args.release_file.clone(), e.to_string()))?;
    let document = verify_release(&bytes, &args.public_key)
        .map_err(|e| VerifyReleaseError::Trust(args.release_file.clone(), e))?;
    Ok(inventory(&document))
}

pub fn inventory(document: &ReleaseDocument) -> ReleaseInventory {
    let mut targets = BTreeSet::new();
    for host in document.deployment_set.hosts.values() {
        for plane in host.planes.values() {
            targets.insert(plane.artifact.target.clone());
            if let Some(ReleaseBootSpec::Managed { roles }) = &plane.boot {
                targets.insert(roles.primary.artifact.target.clone());
                if let Some(nixrescue) = &roles.nixrescue {
                    targets.insert(nixrescue.artifact.target.clone());
                }
            }
        }
    }
    ReleaseInventory {
        version: 1,
        deployment_set_id: document.deployment_set_id.clone(),
        parent_deployment_set_id: document.parent_deployment_set_id.clone(),
        published_at: document.published_at.clone(),
        targets: targets.into_iter().collect(),
    }
}

pub fn parse_args(args: &[String]) -> Result<VerifyReleaseArgs, VerifyReleaseError> {
    let mut release_file = None;
    let mut public_key = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (args[i].as_str(), None),
        };
        match flag {
            "--release" => {
                release_file = Some(PathBuf::from(value_of(args, &mut i, flag, inline)?))
            }
            "--public-key" => public_key = Some(value_of(args, &mut i, flag, inline)?),
            other => {
                return Err(VerifyReleaseError::Usage(format!(
                    "unknown flag {:?} -- see `nixdeploy verify-release` usage",
                    other
                )))
            }
        }
        i += 1;
    }
    let public_key = public_key
        .ok_or_else(|| VerifyReleaseError::Usage("--public-key is required".to_string()))?;
    if public_key.trim().is_empty() {
        return Err(VerifyReleaseError::Usage(
            "--public-key must not be empty".to_string(),
        ));
    }
    Ok(VerifyReleaseArgs {
        release_file: release_file
            .ok_or_else(|| VerifyReleaseError::Usage("--release is required".to_string()))?,
        public_key,
    })
}

fn value_of(
    args: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<String>,
) -> Result<String, VerifyReleaseError> {
    match inline {
        Some(value) => Ok(value),
        None => {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| VerifyReleaseError::Usage(format!("{} needs a value", flag)))
        }
    }
}

pub const USAGE: &str = "\
nixdeploy verify-release --release FILE --public-key KEY

Verify one signed schema-v4 release and print a JSON inventory of every Nix store root it
names. Reads no signing key, builds nothing and changes no release state.";
