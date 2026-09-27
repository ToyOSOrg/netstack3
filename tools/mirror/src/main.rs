//! The mirror's two jobs: holding `upstream/` to Google's bytes, and
//! refreshing it from a named Fuchsia commit.
//!
//! **`upstream/` is Fuchsia's tree, not a fork of it.** Nothing under it is
//! ever edited; our packaging — every Cargo manifest in `crates/` and the empty
//! `fidl_fuchsia_net_common` stand-in — lives beside it. `upstream.toml` names
//! the Fuchsia commit, the rule that picks the files (each crate directory's
//! `BUILD.gn` and everything under its `src/`, and the root files named whole),
//! and every file's git blob id as Fuchsia's own tree names it at that commit.
//!
//! - `check` (and `cargo test`) holds the tree to those ids offline: an edited,
//!   added or missing file is red by name, and so is a crate of ours whose
//!   library is not a pinned file. A blob id is upstream's own name for the
//!   bytes, so anyone can check a pin against `git ls-tree` in a Fuchsia
//!   checkout without trusting this repository.
//! - `sync <commit>` is the only writer: it fetches that one commit's trees
//!   into `target/fuchsia` (depth one, blobs only for the paths the rule
//!   picks), replaces `upstream/` with the files the rule selects, checks the
//!   copy, and rewrites the pins. It asks the network; `check` never does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// Where upstream's files live, relative to the repository root.
const MIRROR: &str = "upstream";

/// The pin, relative to the repository root.
const MANIFEST: &str = "upstream.toml";

/// Our packaging of the mirrored files, relative to the repository root.
const CRATES: &str = "crates";

/// What [`MANIFEST`] holds.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// The repository the commit is fetched from.
    remote: String,
    /// The Fuchsia commit every file is taken from.
    commit: String,
    /// Files at the root of Fuchsia's tree, taken whole.
    files: Vec<String>,
    /// Crate directories: each one's `BUILD.gn` and everything under `src/`.
    crates: Vec<String>,
    /// Every mirrored path and its git blob id at `commit`.
    blobs: BTreeMap<String, String>,
}

impl Manifest {
    fn read(root: &Path) -> Result<Self, String> {
        let path = root.join(MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))
    }

    fn write(&self, root: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| format!("{MANIFEST}: {e}"))?;
        let head = "# Written by `cargo run -p netstack3-mirror -- sync <commit>` (tools/mirror); never by hand.\n\n";
        std::fs::write(root.join(MANIFEST), format!("{head}{text}")).map_err(|e| format!("write {MANIFEST}: {e}"))
    }

    /// Whether the rule picks `path`, a path in Fuchsia's tree.
    fn selects(&self, path: &str) -> bool {
        self.files.iter().any(|f| f == path)
            || self.crates.iter().any(|dir| {
                path.strip_prefix(dir.as_str())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|rest| rest == "BUILD.gn" || rest.starts_with("src/"))
            })
    }

    /// The rule alone, with no pins.
    fn rule(&self, commit: String) -> Self {
        Self {
            remote: self.remote.clone(),
            commit,
            files: self.files.clone(),
            crates: self.crates.clone(),
            blobs: BTreeMap::new(),
        }
    }
}

/// Git's name for a file's bytes: SHA-1 over `blob <len>\0` and the bytes, as
/// `git hash-object --no-filters` computes it.
fn blob_id(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything wrong with a mirror holding `found` — each path and the blob id
/// its bytes hash to — against `manifest`, one line each; empty when the mirror
/// is upstream's.
fn judge(manifest: &Manifest, found: &BTreeMap<String, String>) -> Vec<String> {
    let mut wrong = Vec::new();
    for (path, id) in found {
        match manifest.blobs.get(path) {
            None => wrong.push(format!("{MIRROR}/{path} is no file of Fuchsia {}'s that {MANIFEST} pins", manifest.commit)),
            Some(pinned) if pinned != id => wrong.push(format!(
                "{MIRROR}/{path} is edited: its bytes are blob {id}, and upstream's at {} are {pinned}",
                manifest.commit
            )),
            Some(_) => {}
        }
    }
    for path in manifest.blobs.keys() {
        if !found.contains_key(path) {
            wrong.push(format!("{MIRROR}/{path} is pinned and missing"));
        }
        if !manifest.selects(path) {
            wrong.push(format!("{MANIFEST} pins {path}, which its own rule does not pick"));
        }
    }
    wrong
}

/// Every file under `at`, relative to `dir` with `/` separators.
fn files_under(dir: &Path, at: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(at).map_err(|e| format!("read {}: {e}", at.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("read {}: {e}", at.display()))?.path();
        if path.is_dir() {
            files_under(dir, &path, out)?;
        } else {
            let rel = path.strip_prefix(dir).expect("under the walk's root");
            out.push(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
        }
    }
    Ok(())
}

/// Every file in the mirror and the blob id its bytes hash to.
fn found(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let dir = root.join(MIRROR);
    let mut paths = Vec::new();
    files_under(&dir, &dir, &mut paths)?;
    paths
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(dir.join(&p)).map_err(|e| format!("read {MIRROR}/{p}: {e}"))?;
            Ok((p, blob_id(&bytes)))
        })
        .collect()
}

/// Every crate of ours whose library is not a pinned file of the mirror, one
/// line each: a manifest in `crates/` cannot build bytes that are not
/// upstream's. The stand-in builds a file of its own, under its own `src/`.
fn packaging(root: &Path, manifest: &Manifest) -> Result<Vec<String>, String> {
    let mut wrong = Vec::new();
    let dir = root.join(CRATES);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for krate in entries {
        let path = krate.join("Cargo.toml");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let doc: toml::Value = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
        let lib = doc.get("lib").and_then(|l| l.get("path")).and_then(|p| p.as_str()).unwrap_or("src/lib.rs");
        match lib.strip_prefix("../../upstream/") {
            Some(file) if manifest.blobs.contains_key(file) => {}
            Some(file) => wrong.push(format!("{} builds {file}, which is not pinned", path.display())),
            None if lib.starts_with("src/") && krate.ends_with("fidl_fuchsia_net_common") => {}
            None => wrong.push(format!("{} builds {lib}, neither upstream's nor the stand-in's", path.display())),
        }
    }
    Ok(wrong)
}

/// Both checks, as one report.
fn check(root: &Path) -> Result<String, String> {
    let manifest = Manifest::read(root)?;
    let found = found(root)?;
    let mut wrong = judge(&manifest, &found);
    wrong.extend(packaging(root, &manifest)?);
    if wrong.is_empty() {
        Ok(format!("{MIRROR}/ is Fuchsia {}'s bytes: {} files, each at its pinned blob", manifest.commit, found.len()))
    } else {
        Err(wrong.join("\n"))
    }
}

/// `git <args>` in `dir`, its stdout, or both streams on a refusal.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("run git in {}: {e}", dir.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
    Err(format!("git {} (in {})\n{stdout}\n{stderr}", args.join(" "), dir.display()))
}

/// Replace the mirror with Fuchsia `commit`'s files under the manifest's rule,
/// and pin them.
fn sync(root: &Path, commit: &str) -> Result<String, String> {
    let old = Manifest::read(root)?;
    let dir = root.join("target/fuchsia");
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        git(&dir, &["init", "--quiet"])?;
        git(&dir, &["remote", "add", "origin", &old.remote])?;
    }
    // One commit's trees, and a blob only when the checkout below needs it.
    git(&dir, &["fetch", "--quiet", "--depth", "1", "--filter=blob:none", "origin", commit])?;
    let mut sparse = vec!["sparse-checkout", "set", "--cone", "--"];
    sparse.extend(old.crates.iter().map(String::as_str));
    git(&dir, &sparse)?;
    git(&dir, &["checkout", "--quiet", "--detach", commit])?;
    let resolved = git(&dir, &["rev-parse", "HEAD"])?;

    let mut listing_args = vec!["ls-tree", "-r", "HEAD", "--"];
    listing_args.extend(old.files.iter().map(String::as_str));
    listing_args.extend(old.crates.iter().map(String::as_str));
    let listing = git(&dir, &listing_args)?;
    let mut blobs = BTreeMap::new();
    for line in listing.lines() {
        let (meta, path) = line.split_once('\t').ok_or_else(|| format!("ls-tree printed {line:?}"))?;
        let [mode, kind, id] = meta.split(' ').collect::<Vec<_>>()[..] else {
            return Err(format!("ls-tree printed {line:?}"));
        };
        if !old.selects(path) {
            continue;
        }
        if (mode, kind) != ("100644", "blob") {
            return Err(format!("{path} is a {kind} of mode {mode} at {resolved}; the mirror holds plain files alone"));
        }
        blobs.insert(path.to_string(), id.to_string());
    }
    for file in &old.files {
        if !blobs.contains_key(file) {
            return Err(format!("Fuchsia {resolved} has no {file}"));
        }
    }
    for krate in &old.crates {
        if !blobs.keys().any(|p| p.starts_with(&format!("{krate}/src/"))) {
            return Err(format!("Fuchsia {resolved} has no {krate}/src"));
        }
    }

    let mirror = root.join(MIRROR);
    if mirror.exists() {
        std::fs::remove_dir_all(&mirror).map_err(|e| format!("remove {}: {e}", mirror.display()))?;
    }
    for path in blobs.keys() {
        let to = mirror.join(path);
        std::fs::create_dir_all(to.parent().expect("a file has a directory"))
            .map_err(|e| format!("create {}: {e}", to.display()))?;
        std::fs::copy(dir.join(path), &to).map_err(|e| format!("copy {path}: {e}"))?;
    }
    let new = Manifest { blobs, ..old.rule(resolved.clone()) };
    let wrong = judge(&new, &found(root)?);
    if !wrong.is_empty() {
        return Err(format!("the copied mirror is not the fetched tree:\n{}", wrong.join("\n")));
    }
    new.write(root)?;
    let moved = new.blobs.iter().filter(|(p, id)| old.blobs.get(*p) != Some(id)).count();
    let gone = old.blobs.keys().filter(|p| !new.blobs.contains_key(*p)).count();
    Ok(format!(
        "{MIRROR}/ is Fuchsia {resolved}: {} files, {moved} new or changed and {gone} gone since {}",
        new.blobs.len(),
        old.commit
    ))
}

/// The repository this tool is part of.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo_root();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["check"] => check(&root),
        ["sync", commit] => sync(&root, commit),
        _ => Err("usage: netstack3-mirror check | sync <fuchsia commit>".to_string()),
    };
    match result {
        Ok(said) => println!("{said}"),
        Err(why) => {
            eprintln!("netstack3-mirror: {why}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The mirror is Fuchsia's bytes**, and every crate of ours builds one of
    /// them.
    #[test]
    fn the_mirror_is_upstreams_bytes() {
        let root = repo_root();
        let found = found(&root).expect("the mirror hashes");
        assert!(found.len() > 200, "the walk found {} files, so it is reading no mirror", found.len());
        check(&root).unwrap_or_else(|wrong| panic!("{wrong}"));
    }

    /// The hash is git's: `git hash-object` of an empty file and of `hello\n`.
    #[test]
    fn a_blob_id_is_gits() {
        assert_eq!(blob_id(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(blob_id(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    fn manifest() -> Manifest {
        Manifest {
            remote: "r".into(),
            commit: "c".into(),
            files: vec!["LICENSE".into()],
            crates: vec!["a/b".into()],
            blobs: [("LICENSE", "1"), ("a/b/BUILD.gn", "2"), ("a/b/src/lib.rs", "3")]
                .into_iter()
                .map(|(p, i)| (p.to_string(), i.to_string()))
                .collect(),
        }
    }

    /// The judge's own fixture: an edit, an arrival, a loss and a pin outside
    /// the rule are each red by name, and the pinned tree is not.
    #[test]
    fn an_edited_added_or_missing_file_is_red() {
        let m = manifest();
        assert!(judge(&m, &m.blobs).is_empty());

        let mut edited = m.blobs.clone();
        edited.insert("a/b/src/lib.rs".into(), "9".into());
        let wrong = judge(&m, &edited);
        assert!(wrong.len() == 1 && wrong[0].contains("a/b/src/lib.rs is edited"), "{wrong:?}");

        let mut added = m.blobs.clone();
        added.insert("a/b/src/ours.rs".into(), "4".into());
        assert!(judge(&m, &added)[0].contains("no file of Fuchsia"));

        let mut lost = m.blobs.clone();
        lost.remove("a/b/BUILD.gn");
        assert!(judge(&m, &lost)[0].contains("pinned and missing"));

        let mut outside = manifest();
        outside.blobs.insert("a/b/OWNERS".into(), "5".into());
        assert!(judge(&outside, &outside.blobs)[0].contains("does not pick"));
    }

    #[test]
    fn the_rule_picks_build_files_sources_and_named_root_files() {
        let m = manifest();
        assert!(m.selects("LICENSE"));
        assert!(m.selects("a/b/BUILD.gn"));
        assert!(m.selects("a/b/src/deep/x.rs"));
        assert!(!m.selects("a/b/OWNERS"));
        assert!(!m.selects("a/bc/src/x.rs"));
        assert!(!m.selects("a/b/tests/x.rs"));
        assert!(!m.selects("PATENTS"));
    }
}
