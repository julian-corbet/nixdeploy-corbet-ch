//! Sizes a change against THIS machine's own store, without downloading or evaluating
//! anything -- the mechanism `README.md`'s "Why the receiver decides" section is about.
//!
//! A sender cannot size a change correctly: it would have to keep its own model of what
//! every managed machine's store currently holds, and that model is wrong exactly when it
//! matters (after an unclean run, a garbage collection, or a restore on the receiving
//! machine). So this module asks the machine's actual store what it already has, and asks
//! the substituter for `.narinfo` metadata -- never a NAR body -- for whatever is missing.
//! `NarSize` from that metadata is the true byte cost of fetching a path; nothing here ever
//! downloads a path just to measure it.
//!
//! `compute` and its two dependencies (`LocalStore`, `NarinfoSource`) are split into traits
//! so the closure-walk and byte-summing logic can be unit-tested against fakes, entirely
//! offline, without a real Nix store or network access -- the real implementations
//! (`NixStore`, `HttpNarinfoSource`) are thin and untested-by-design; the arithmetic and
//! walk order are what actually needs to be right.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::process::Command;

use crate::manifest::FETCH_TIMEOUT;

/// Nix's default store directory. Not configurable here: a receiver running a non-default
/// store directory is a genuinely unusual setup this crate does not claim to support, and
/// guessing at a knob nobody asked for would be exactly the kind of invented behaviour this
/// project avoids elsewhere.
pub const DEFAULT_STORE_DIR: &str = "/nix/store";

/// The two `.narinfo` fields this module actually needs. Everything else in a narinfo
/// (`URL`, `Compression`, `FileHash`, `Deriver`, `Sig`, ...) is either irrelevant to sizing
/// a change or is the publisher's/nix's own concern once activation actually fetches the
/// path, not this receiver's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narinfo {
    pub nar_size: u64,
    /// Full store paths (already joined with the store directory), not bare basenames --
    /// see `parse_narinfo`.
    pub references: Vec<String>,
}

/// Answers "is this store path already present, valid, and (by the store's own invariant)
/// therefore complete down to its own references?" A store never holds a valid path whose
/// own dependencies are missing, so `compute` relies on this to prune the closure walk: once
/// a path is found present, nothing below it needs to be asked about at all.
///
/// Three answers, not two. "The store says no" and "the store could not be asked" must never
/// collapse into the same `false`: see `classify_path_info` for the machine that gets
/// destroyed when they do.
pub trait LocalStore {
    fn is_present(&self, store_path: &str) -> Result<bool, DeltaError>;
}

/// Fetches `.narinfo` metadata for a store path that is NOT present locally. Implementations
/// must never fetch the NAR body itself -- see the module doc.
pub trait NarinfoSource {
    fn fetch(&self, store_path: &str) -> Result<Narinfo, DeltaError>;
}

/// The result of sizing a change: total new bytes, and which paths they came from.
///
/// `missing` is not reported anywhere -- `receive.rs` reads only `bytes`, and
/// `Outcome::Refused` deliberately carries a fixed, small payload (see `outcome.rs` on why
/// nothing per-run-variable belongs in a value a metrics sink turns into a series). It is
/// populated because the walk has the list in hand anyway, and it is what this module's own
/// tests assert the walk pruned correctly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delta {
    pub bytes: u64,
    pub missing: Vec<String>,
}

#[derive(Debug)]
pub enum DeltaError {
    LocalStoreQuery(String, String),
    NarinfoFetch(String, String),
    /// A narinfo that failed to parse. Always an error, NEVER treated as a zero-byte path --
    /// silently skipping an unparseable narinfo would let an arbitrarily large path through
    /// the ceiling check for free, which is precisely the failure a size ceiling exists to
    /// prevent.
    NarinfoParse(String, String),
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::LocalStoreQuery(path, e) => {
                write!(f, "querying local store for {}: {}", path, e)
            }
            DeltaError::NarinfoFetch(path, e) => write!(f, "fetching narinfo for {}: {}", path, e),
            DeltaError::NarinfoParse(path, e) => write!(f, "parsing narinfo for {}: {}", path, e),
        }
    }
}

impl std::error::Error for DeltaError {}

/// Walks the closure of `target`, starting only from `target` itself -- never from a
/// separately-supplied reference list, because a local store that has never seen `target`
/// has no way to know its references without asking the substituter, which is exactly what
/// this function does for every path it finds missing.
///
/// A path already present locally is never descended into (see `LocalStore`'s doc): this is
/// what keeps the walk bounded by the NEW part of the closure, not the whole tree, on every
/// run after the first.
pub fn compute(
    target: &str,
    store: &dyn LocalStore,
    narinfo: &dyn NarinfoSource,
) -> Result<Delta, DeltaError> {
    let mut total: u64 = 0;
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();

    queue.push_back(target.to_string());
    seen.insert(target.to_string());

    while let Some(path) = queue.pop_front() {
        if store.is_present(&path)? {
            continue;
        }

        let info = narinfo.fetch(&path)?;
        // Saturating rather than panicking on overflow: a real closure will never approach
        // u64::MAX bytes, so this only ever fires on a corrupt/adversarial narinfo, and
        // saturating just guarantees such a thing reads as "way over ceiling" instead of
        // wrapping around to something that looks small.
        total = total.saturating_add(info.nar_size);
        missing.push(path.clone());

        for reference in info.references {
            if seen.insert(reference.clone()) {
                queue.push_back(reference);
            }
        }
    }

    Ok(Delta {
        bytes: total,
        missing,
    })
}

/// The one comparison `nixdeploy.receiver.maxInplaceDeltaBytes` exists for. `None` means no
/// ceiling -- see that option's doc in `modules/default.nix`: a deliberate "this machine is
/// large enough that size is not a survival question," not a placeholder for "not yet
/// tuned," so it must never cause a refusal.
pub fn exceeds_ceiling(bytes: u64, ceiling: Option<u64>) -> bool {
    ceiling.is_some_and(|limit| bytes > limit)
}

/// Queries the machine's OWN Nix store via the `nix` CLI. This is a query against the
/// store's database, not an evaluation and not a build -- no flake is loaded, nothing is
/// derived, nothing is fetched; it answers exactly one question, "is this path already
/// registered valid here."
pub struct NixStore {
    /// Absolute path or bare name of the `nix` binary to invoke -- see
    /// `ReceiverConfig::nix_binary` in `receive.rs`.
    pub nix_binary: String,
}

impl LocalStore for NixStore {
    fn is_present(&self, store_path: &str) -> Result<bool, DeltaError> {
        let output = Command::new(&self.nix_binary)
            .args([
                "--extra-experimental-features",
                "nix-command",
                "path-info",
                store_path,
            ])
            .output()
            .map_err(|e| DeltaError::LocalStoreQuery(store_path.to_string(), e.to_string()))?;
        classify_path_info(
            store_path,
            output.status.success(),
            &String::from_utf8_lossy(&output.stderr),
        )
        .map_err(|detail| DeltaError::LocalStoreQuery(store_path.to_string(), detail))
    }
}

/// Turns one `nix path-info` result into the THREE answers it can actually carry, rather than
/// the two an exit code has. Split out from `is_present` so the classification is testable
/// without a nix, a store or a daemon on the machine running the tests.
///
/// The exit code alone is not the answer, and reading it as one is the most expensive bug
/// this file can have. Every non-zero exit -- a stopped nix-daemon, a store DB that could not
/// be opened or was locked, a `nixBinary` too old or too new for the store it was pointed at
/// (`modules/default.nix`'s `nixBinary` doc anticipates exactly that version skew on
/// system-manager machines) -- used to read as "this path is not here". So a receiver that
/// could not query its store at all measured its ENTIRE target closure as missing, blew
/// through `maxInplaceDeltaBytes`, and `route_over_ceiling` asked the provider to destroy and
/// replace a machine whose store already held nearly everything it needed. An unanswerable
/// query is a failed run that the next tick retries; that is the direction where the cost of
/// being wrong is one skipped interval instead of one machine.
///
/// Absence is therefore recognised POSITIVELY, from nix's own wording, and narrowly: the
/// message must both name the path that was asked about and say it is not there. Matching
/// loosely would put the hazard straight back -- a dead daemon reports "cannot connect to
/// socket at '...': No such file or directory", which any "file not found" style match would
/// happily read as an absent path.
fn classify_path_info(store_path: &str, success: bool, stderr: &str) -> Result<bool, String> {
    if success {
        return Ok(true);
    }
    let names_the_path = stderr.contains(store_path);
    let says_absent = ABSENCE_PHRASES.iter().any(|p| stderr.contains(p));
    if names_the_path && says_absent {
        return Ok(false);
    }
    Err(format!(
        "`nix path-info` exited non-zero without saying the path is absent, so this store \
         could not be queried at all: {}",
        stderr.trim()
    ))
}

/// The wordings nix uses to say a path it was asked about is not in this store. Both spellings
/// are checked because this receiver deliberately runs whatever `nix` the machine has (see
/// `NixStore::nix_binary`), and that has ranged over years of releases; "is not valid" is what
/// current nix prints (verified against Determinate Nix 3.21.9 / 2.34.8), "does not exist" is
/// the older phrasing.
const ABSENCE_PHRASES: [&str; 2] = ["is not valid", "does not exist"];

/// Fetches `.narinfo` files over HTTP(S) from a fixed list of substituter base URLs, trying
/// each in order until one answers. The base URLs come from the SAME substituters this
/// machine's Nix is already configured to trust for everything else (see
/// `substituters_from_nix_config` in `receive.rs`) -- this module intentionally has no
/// substituter option of its own, because one already exists system-wide and duplicating it
/// here would just create a second place for the two to drift apart.
pub struct HttpNarinfoSource {
    pub substituters: Vec<String>,
    pub store_dir: String,
}

impl NarinfoSource for HttpNarinfoSource {
    fn fetch(&self, store_path: &str) -> Result<Narinfo, DeltaError> {
        let hash = store_hash(store_path, &self.store_dir).ok_or_else(|| {
            DeltaError::NarinfoParse(
                store_path.to_string(),
                format!(
                    "not a store path under {} -- cannot derive a narinfo hash from it",
                    self.store_dir
                ),
            )
        })?;

        let mut last_error: Option<String> = None;
        for base in &self.substituters {
            let url = format!("{}/{}.narinfo", base.trim_end_matches('/'), hash);
            // Bounded for the same reason the manifest fetch is (see `FETCH_TIMEOUT`), plus
            // one this loop owns: a substituter that accepts the connection and then never
            // answers must fall through to the NEXT substituter, which is what this list is
            // for. Unbounded, the first blackholing mirror in it stalls the walk forever and
            // the machine never reaches the substituter that would have answered.
            match ureq::get(&url).timeout(FETCH_TIMEOUT).call() {
                Ok(response) => {
                    let body = response
                        .into_string()
                        .map_err(|e| DeltaError::NarinfoFetch(url.clone(), e.to_string()))?;
                    return parse_narinfo(&body, &self.store_dir).map_err(|detail| {
                        DeltaError::NarinfoParse(store_path.to_string(), detail)
                    });
                }
                Err(e) => {
                    last_error = Some(format!("{}: {}", url, e));
                    continue;
                }
            }
        }

        Err(DeltaError::NarinfoFetch(
            store_path.to_string(),
            last_error.unwrap_or_else(|| "no substituters configured".to_string()),
        ))
    }
}

/// Extracts the 32-character nix-base32 hash prefix a narinfo is named after, e.g.
/// `/nix/store/abcd...wxyz-hello-2.12` -> `abcd...wxyz`.
fn store_hash<'a>(store_path: &'a str, store_dir: &str) -> Option<&'a str> {
    let base = store_path.strip_prefix(store_dir)?;
    let base = base.strip_prefix('/')?;
    let (hash, _rest) = base.split_once('-')?;
    if hash.len() == 32 {
        Some(hash)
    } else {
        None
    }
}

/// Parses the subset of the narinfo text format this module needs. Narinfo is a flat
/// `Key: value` list, one per line -- see
/// <https://nixos.org/manual/nix/stable/protocols/tarball-fetcher.html> for the fuller
/// format; only `NarSize` and `References` matter here.
fn parse_narinfo(body: &str, store_dir: &str) -> Result<Narinfo, String> {
    let mut nar_size: Option<u64> = None;
    let mut references: Vec<String> = Vec::new();

    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "NarSize" => {
                nar_size = Some(
                    value
                        .parse::<u64>()
                        .map_err(|e| format!("NarSize {:?}: {}", value, e))?,
                );
            }
            "References" => {
                references = value
                    .split_whitespace()
                    .map(|basename| format!("{}/{}", store_dir, basename))
                    .collect();
            }
            _ => {}
        }
    }

    // A leaf path legitimately has no References line at all -- that is not an error. A
    // missing NarSize is: every valid narinfo has one, so its absence means either a
    // truncated response or a substituter answering with something that is not actually a
    // narinfo, and treating that as "0 bytes" would defeat the ceiling entirely.
    let nar_size = nar_size.ok_or_else(|| "missing NarSize field".to_string())?;

    Ok(Narinfo {
        nar_size,
        references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct FakeStore {
        present: HashSet<String>,
    }
    impl LocalStore for FakeStore {
        fn is_present(&self, store_path: &str) -> Result<bool, DeltaError> {
            Ok(self.present.contains(store_path))
        }
    }

    struct FakeNarinfo {
        info: HashMap<String, Narinfo>,
        // Records which paths were actually asked for, so a test can assert a present path
        // was never queried -- proving the "never download/fetch what you already have"
        // pruning actually happens, not just that the final byte total looks right.
        fetched: RefCell<Vec<String>>,
    }
    impl NarinfoSource for FakeNarinfo {
        fn fetch(&self, store_path: &str) -> Result<Narinfo, DeltaError> {
            self.fetched.borrow_mut().push(store_path.to_string());
            self.info.get(store_path).cloned().ok_or_else(|| {
                DeltaError::NarinfoFetch(store_path.to_string(), "no such fake path".into())
            })
        }
    }

    #[test]
    fn sums_only_missing_paths_and_prunes_present_subtrees() {
        // target -> dep_missing -> dep_of_missing (also missing)
        //        -> dep_present (already valid locally; its own reference must never be
        //           fetched, because a valid path's closure is always already complete)
        let target = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target";
        let dep_missing = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-missing";
        let dep_of_missing = "/nix/store/cccccccccccccccccccccccccccccccc-deeper";
        let dep_present = "/nix/store/dddddddddddddddddddddddddddddddd-present";
        let unreachable_below_present = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-unreached";

        let store = FakeStore {
            present: [dep_present.to_string()].into_iter().collect(),
        };

        let mut info = HashMap::new();
        info.insert(
            target.to_string(),
            Narinfo {
                nar_size: 100,
                references: vec![dep_missing.to_string(), dep_present.to_string()],
            },
        );
        info.insert(
            dep_missing.to_string(),
            Narinfo {
                nar_size: 50,
                references: vec![dep_of_missing.to_string()],
            },
        );
        info.insert(
            dep_of_missing.to_string(),
            Narinfo {
                nar_size: 25,
                references: vec![],
            },
        );
        // Deliberately no entry for dep_present or unreachable_below_present: if the walk
        // ever tried to fetch either, FakeNarinfo::fetch would return an Err and fail the
        // test, since a present path must never be descended into.
        let _ = unreachable_below_present;

        let narinfo = FakeNarinfo {
            info,
            fetched: RefCell::new(Vec::new()),
        };

        let delta = compute(target, &store, &narinfo).expect("compute");
        assert_eq!(delta.bytes, 100 + 50 + 25);
        assert_eq!(
            delta.missing.len(),
            3,
            "want target + dep_missing + dep_of_missing, got {:?}",
            delta.missing
        );
        assert!(!narinfo.fetched.borrow().contains(&dep_present.to_string()));
    }

    #[test]
    fn already_present_target_costs_nothing() {
        let target = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target";
        let store = FakeStore {
            present: [target.to_string()].into_iter().collect(),
        };
        let narinfo = FakeNarinfo {
            info: HashMap::new(),
            fetched: RefCell::new(Vec::new()),
        };
        let delta = compute(target, &store, &narinfo).expect("compute");
        assert_eq!(delta.bytes, 0);
        assert!(delta.missing.is_empty());
    }

    #[test]
    fn narinfo_missing_narsize_is_an_error_not_zero() {
        let body = "StorePath: /nix/store/xxxx-name\nReferences: \n";
        let err = parse_narinfo(body, DEFAULT_STORE_DIR).unwrap_err();
        assert!(err.contains("NarSize"), "error was: {}", err);
    }

    #[test]
    fn narinfo_unparseable_narsize_is_an_error_not_zero() {
        let body = "NarSize: not-a-number\n";
        assert!(parse_narinfo(body, DEFAULT_STORE_DIR).is_err());
    }

    #[test]
    fn narinfo_references_join_store_dir() {
        let body = "NarSize: 10\nReferences: aaa-dep1 bbb-dep2\n";
        let info = parse_narinfo(body, "/nix/store").expect("parse");
        assert_eq!(
            info.references,
            vec![
                "/nix/store/aaa-dep1".to_string(),
                "/nix/store/bbb-dep2".to_string(),
            ]
        );
    }

    #[test]
    fn store_hash_extraction() {
        let path = "/nix/store/abcdefghijklmnopqrstuvwxyz012345-hello-2.12";
        assert_eq!(
            store_hash(path, DEFAULT_STORE_DIR),
            Some("abcdefghijklmnopqrstuvwxyz012345")
        );
        assert_eq!(store_hash("/not/the/store/dir-x", DEFAULT_STORE_DIR), None);
    }

    #[test]
    fn a_nix_that_could_not_answer_is_an_error_not_an_absent_path() {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target";

        assert_eq!(
            classify_path_info(path, true, ""),
            Ok(true),
            "exit zero is the only thing that means present"
        );

        // The wording current nix prints for a path this store genuinely does not have
        // (verified against Determinate Nix 3.21.9 / 2.34.8), and the older phrasing.
        assert_eq!(
            classify_path_info(path, false, &format!("error: path '{}' is not valid", path)),
            Ok(false)
        );
        assert_eq!(
            classify_path_info(path, false, &format!("error: path '{}' does not exist", path)),
            Ok(false)
        );

        // The whole reason this function exists: a store that could not be OPENED must never
        // read as a store that does not HAVE the path. Every one of these used to be a
        // silent `false`, which sizes the entire closure as missing and routes a healthy
        // machine to a destructive reimage.
        let unanswerable = [
            // A stopped daemon, or a NIX_REMOTE pointing at a socket that is not there. Note
            // it carries "No such file or directory" -- a looser match on absence would read
            // this as a missing path.
            "error: cannot connect to socket at '/nix/var/nix/daemon-socket/socket': No such \
             file or directory",
            "error: SQLite database '/nix/var/nix/db/db.sqlite' is busy",
            "error: unrecognised flag '--extra-experimental-features'",
            "",
        ];
        for stderr in unanswerable {
            assert!(
                classify_path_info(path, false, stderr).is_err(),
                "a store that could not answer must be an error, not an absent path: {:?}",
                stderr
            );
        }

        // Absence wording that names some OTHER path is not an answer about this one -- e.g. a
        // broken `include` in nix.conf failing before the query is even reached.
        assert!(classify_path_info(
            path,
            false,
            "error: file '/etc/nix/extra.conf' does not exist"
        )
        .is_err());
    }

    #[test]
    fn ceiling_comparison() {
        assert!(
            !exceeds_ceiling(1_000_000, None),
            "no ceiling never refuses"
        );
        assert!(
            !exceeds_ceiling(100, Some(100)),
            "exactly at the ceiling is not over it"
        );
        assert!(exceeds_ceiling(101, Some(100)));
    }
}
