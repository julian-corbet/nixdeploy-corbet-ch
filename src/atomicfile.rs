//! One way to put a file where a reader that never coordinates with the writer will find it
//! either whole or not at all.
//!
//! Both files this crate writes are read by something that scrapes or fetches on its own
//! schedule and cannot be asked to wait: a Prometheus textfile collector (`metrics.rs`) and
//! whatever static origin serves a published manifest (`publish.rs`). A plain
//! `File::create` + `write_all` is two observable states with a window between them, and in
//! that window the file exists and is WRONG -- a truncated exposition (which makes a
//! collector discard the entire file, not just the missing lines) or a truncated manifest
//! (which every receiver in the fleet then refuses, correctly, all at once).
//!
//! `rename(2)` inside a single directory is atomic, so a temp file in the SAME directory
//! followed by a rename has no such window. Same directory is not a detail: a rename across
//! filesystems is not a rename at all, it is a copy, and the window comes straight back.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process;

/// The directory the temp file goes in: the target's own directory, so the rename that
/// follows stays inside one filesystem.
///
/// A bare file name (`--out manifest.json`) has a parent, and it is the EMPTY path rather than
/// `None` -- `Path::new("manifest.json").parent()` is `Some("")`. Reading that as "no
/// directory" made every relative output path an error, which is the ordinary way a publisher
/// is invoked from the directory the manifest belongs in. The current directory is that
/// target's directory, so `.` is the correct answer and the atomicity argument above is
/// unaffected. `None` is still a real refusal: it means the path is a root (`/`) or ends in
/// `..`, neither of which names a file to write.
fn temp_dir_for(path: &Path) -> Result<&Path, String> {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(p) => Ok(p),
        None => Err(format!(
            "{} has no parent directory to write a temp file in",
            path.display()
        )),
    }
}

/// Writes `bytes` to `path` via a temp file in the same directory, then renames it into
/// place with `mode`. On any failure the temp file is removed rather than left behind, so a
/// directory something else reads does not accumulate half-written dot-files.
///
/// The temp name carries this process's PID because two writers can legitimately run at once
/// -- a timer-driven receiver and an operator running the same binary by hand -- and a
/// shared temp name would have one of them rename a file the other was still writing into,
/// which reintroduces exactly the torn read the rename exists to prevent.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let dir = temp_dir_for(path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{} does not name a file", path.display()))?
        .to_string_lossy()
        .to_string();
    let tmp = dir.join(format!(".{}.{}.tmp", file_name, process::id()));

    let write = || -> Result<(), String> {
        let mut file =
            fs::File::create(&tmp).map_err(|e| format!("creating {}: {}", tmp.display(), e))?;
        file.write_all(bytes)
            .map_err(|e| format!("writing {}: {}", tmp.display(), e))?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| format!("setting mode on {}: {}", tmp.display(), e))?;
        // Without this, the rename can land before the data does, and a machine that loses
        // power in between comes back with a file that exists and is empty.
        file.sync_all()
            .map_err(|e| format!("syncing {}: {}", tmp.display(), e))?;
        fs::rename(&tmp, path)
            .map_err(|e| format!("renaming {} to {}: {}", tmp.display(), path.display(), e))
    };

    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nixdeploy-atomic-{}-{}", tag, process::id()));
        fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    #[test]
    fn replaces_an_existing_file_and_leaves_nothing_else_in_the_directory() {
        let dir = tmpdir("replace");
        let path = dir.join("target");
        fs::write(&path, b"old").expect("seed");

        write_atomic(&path, b"new", 0o644).expect("write");

        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        assert_eq!(
            fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
            0o644
        );
        let names: Vec<String> = fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["target".to_string()],
            "the temp file must be renamed away, not left next to the target: {:?}",
            names
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bare_relative_name_resolves_to_the_current_directory() {
        // `nixdeploy publish --out manifest.json`, run from the directory the manifest
        // belongs in, is the ordinary invocation. `Path::parent` answers `Some("")` for it,
        // and reading that as "no directory" made the whole publisher unusable that way --
        // it refused before writing anything, with a message about a parent directory the
        // operator had not asked for. Asserted on the pure resolution rather than by
        // chdir'ing, because `set_current_dir` is process-global and these tests run in
        // parallel threads.
        assert_eq!(
            temp_dir_for(Path::new("manifest.json")).expect("a bare file name is writable"),
            Path::new(".")
        );
        assert_eq!(
            temp_dir_for(Path::new("out/manifest.json")).unwrap(),
            Path::new("out")
        );
        assert_eq!(
            temp_dir_for(Path::new("/var/lib/collector/nixdeploy.prom")).unwrap(),
            Path::new("/var/lib/collector")
        );
        // A path that names no file at all is still a refusal.
        assert!(temp_dir_for(Path::new("/")).is_err());
    }

    #[test]
    fn a_failed_write_reports_the_reason_and_creates_nothing() {
        let err = write_atomic(
            Path::new("/nonexistent-nixdeploy-dir/deeper/file"),
            b"x",
            0o644,
        )
        .unwrap_err();
        assert!(err.contains("creating"), "got: {}", err);
        assert!(!Path::new("/nonexistent-nixdeploy-dir").exists());
    }
}
