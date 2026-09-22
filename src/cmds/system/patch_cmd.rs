//! `rtk patch` — apply a unified diff, all-or-nothing, with a cheap receipt.
//!
//! Two shapes:
//! - `rtk patch <file> < changes.diff`           — single file, target on the CLI
//! - `rtk patch [-p N] [--backup] < multi.patch` — multi-file git-format diff;
//!   each file's target is read from its `+++ ` header (falling back to `--- `
//!   for deletions), with `-p N` prefix stripping (default: 1 for `a/`/`b/`).
//!
//! All-or-nothing holds for the whole validation+apply phase: every section is
//! parsed and applied to an in-memory final image before ANY file is touched, so
//! a single failed/mismatched hunk — or an unsafe path — writes zero bytes.
//! Fidelity matches `rtk edit`: per-file CRLF/UTF-8/BOM/GBK preserved, atomic
//! write, `--backup` keeps `<name>.bak`, `--dry-run` rehearses with no writes.
//! `/dev/null` on the old side creates a file; on the new side it deletes one.

use crate::core::edit::{
    self, RECEIPT_CAP_LINES, TextDoc, backup_file, diff_block, reject_git_internal,
};
use crate::core::tracking;
use anyhow::Result;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Exit codes shared with `rtk edit`: 0 ok · 1 a section failed to apply (or an
/// unsafe path) — nothing written · 2 usage/parse · 3 IO failure.
pub fn run(
    file: Option<PathBuf>,
    dry_run: bool,
    backup: bool,
    strip: Option<usize>,
    verbose: u8,
) -> Result<i32> {
    // A positional target is validated before we block on stdin (refuse early).
    if let Some(f) = &file
        && let Err(e) = reject_git_internal(f)
    {
        eprintln!("rtk patch: {e}");
        return Ok(2);
    }

    let mut patch_in = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut patch_in) {
        eprintln!("rtk patch: cannot read stdin: {e}");
        return Ok(2);
    }
    if patch_in.trim().is_empty() {
        eprintln!("rtk patch: no unified diff on stdin (usage: rtk patch [file] < changes.diff)");
        return Ok(2);
    }

    let mut sections = match parse_sections(&patch_in.replace("\r\n", "\n")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtk patch: {e}");
            return Ok(2);
        }
    };
    if sections.is_empty() {
        // Headerless hunk-only diff: only meaningful against a pinned target.
        let Some(f) = &file else {
            eprintln!(
                "rtk patch: diff has no ---/+++ headers; give a target file for a hunk-only patch"
            );
            return Ok(2);
        };
        sections.push(Section {
            old_path: None,
            new_path: None,
            patch: patch_in.replace("\r\n", "\n"),
        });
        return match apply_section(Some(f.clone()), &sections[0], strip, verbose) {
            Ok(p) => finish(vec![p], dry_run, backup, verbose),
            Err(e) => {
                eprintln!("rtk patch: {e} — nothing written");
                Ok(1)
            }
        };
    }

    // Positional target pins a single section; multi-section needs header paths.
    if let Some(f) = &file {
        if sections.len() != 1 {
            eprintln!(
                "rtk patch: {} sections in the diff but a single target was given — \
                 drop the file argument to patch by header path",
                sections.len()
            );
            return Ok(1);
        }
        return match apply_section(Some(f.clone()), &sections[0], strip, verbose) {
            Ok(p) => finish(vec![p], dry_run, backup, verbose),
            Err(e) => {
                eprintln!("rtk patch: {e} — nothing written");
                Ok(1)
            }
        };
    }

    let mut prepared = Vec::with_capacity(sections.len());
    for sec in &sections {
        match apply_section(None, sec, strip, verbose) {
            Ok(p) => prepared.push(p),
            Err(e) => {
                // Nothing has been written yet (writes happen in `finish`).
                eprintln!("rtk patch: {e} — nothing written");
                return Ok(1);
            }
        }
    }
    finish(prepared, dry_run, backup, verbose)
}

/// One file's diff: the raw `--- `/`+++ ` paths plus the re-assembled single-file
/// patch text (headers + hunks) that `diffy` can parse.
struct Section {
    old_path: Option<String>,
    new_path: Option<String>,
    patch: String,
}

/// What `apply_section` decided to do with one file.
struct Prepared {
    target: PathBuf,
    /// Some(bytes): write; None with `deletes`: remove; both-None: no-op new file.
    final_bytes: Option<Vec<u8>>,
    deletes: bool,
    hunks: usize,
    added: usize,
    removed: usize,
    body: String,
    /// Pre-edit bytes for rollback (None for a create): the write phase restores
    /// these if a later file's write fails, keeping the whole patch atomic.
    original_bytes: Option<Vec<u8>>,
}

/// Parse a multi-file unified diff into per-file sections. A `--- `/`+++ ` pair
/// only starts a new section when we are NOT inside a hunk (line counters
/// drained), so a `-`/`+` hunk line whose text happens to start with `---`/`+++`
/// is never mistaken for a header.
fn parse_sections(diff: &str) -> Result<Vec<Section>, String> {
    let mut sections: Vec<Section> = Vec::new();
    let mut cur: Option<Section> = None;
    let mut in_hunk = false;
    let (mut old_left, mut new_left) = (0usize, 0usize);

    for line in diff.lines() {
        let outside_hunk = !in_hunk;
        match (outside_hunk, line.split_once(' ')) {
            (true, Some(("---", _))) | (true, Some(("+++", _))) => {
                // File header pair, and we are NOT inside a hunk. A `---` here
                // starts a new section (flushing any prior one) — plain diffs
                // have no `diff --git` marker, so `---` is the only delimiter
                // between two files. A `+++` just records the new path on the
                // current section (creating one if a diff opens with it).
                if line.starts_with("--- ") {
                    if let Some(s) = cur.take()
                        && !s.patch.is_empty()
                    {
                        sections.push(s);
                    }
                    cur = Some(Section {
                        old_path: None,
                        new_path: None,
                        patch: String::new(),
                    });
                }
                let sec = cur.get_or_insert_with(|| Section {
                    old_path: None,
                    new_path: None,
                    patch: String::new(),
                });
                if let Some(p) = header_path(line) {
                    if line.starts_with("---") {
                        sec.old_path = Some(p);
                    } else {
                        sec.new_path = Some(p);
                    }
                }
                sec.patch.push_str(line);
                sec.patch.push('\n');
            }
            (true, Some(("diff", _))) => {
                // `diff --git a/x b/x` — git preamble: flush prior, start fresh.
                // NOT appended to the patch body: diffy rejects it as trailing
                // content before the first hunk. The `--- `/`+++ ` pair that
                // follows carries the paths diffy needs.
                if let Some(s) = cur.take()
                    && !s.patch.is_empty()
                {
                    sections.push(s);
                }
                cur = Some(Section {
                    old_path: None,
                    new_path: None,
                    patch: String::new(),
                });
            }
            (_, Some((_, _))) if line.starts_with("@@") => {
                let (o, n) = hunk_counts(line)?;
                old_left = o;
                new_left = n;
                in_hunk = true;
                if let Some(s) = cur.as_mut() {
                    s.patch.push_str(line);
                    s.patch.push('\n');
                }
                continue;
            }
            _ => {}
        }

        // Body lines: only meaningful inside a hunk; drain the counters.
        if in_hunk {
            match line.chars().next() {
                Some(' ') | Some(':') => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                Some('-') => old_left = old_left.saturating_sub(1),
                Some('+') => new_left = new_left.saturating_sub(1),
                Some('\\') => {} // "\ No newline at end of file" — no line
                _ => {}
            }
            if let Some(s) = cur.as_mut() {
                s.patch.push_str(line);
                s.patch.push('\n');
            }
            if old_left == 0 && new_left == 0 {
                in_hunk = false;
            }
        }
    }
    if let Some(s) = cur.take()
        && !s.patch.is_empty()
    {
        sections.push(s);
    }
    Ok(sections)
}

/// Path from a `--- a/foo<TAB>2024...` / `+++ b/foo` header, trimming the
/// optional timestamp after a tab. `/dev/null` is preserved.
fn header_path(line: &str) -> Option<String> {
    let rest = line.split_once(' ')?.1;
    let p = rest.split('\t').next().unwrap_or("");
    (!p.is_empty()).then(|| p.to_string())
}

/// `(old_len, new_len)` from `@@ -l,s +l,s @@` (each length defaults to 1).
fn hunk_counts(line: &str) -> Result<(usize, usize), String> {
    let malformed = || format!("malformed hunk header: {line}");
    let inner = line
        .split_once("@@ ")
        .and_then(|(_, r)| r.split_once(" @@"))
        .map(|(m, _)| m)
        .ok_or_else(malformed)?;
    let mut toks = inner.split_whitespace();
    let length_of = |tok: Option<&str>| {
        let t = tok?.trim_start_matches(['-', '+']);
        Some(match t.split_once(',') {
            Some((_, len)) => len.parse().unwrap_or(1),
            None => 1,
        })
    };
    let old = length_of(toks.next()).ok_or_else(malformed)?;
    let new = length_of(toks.next()).ok_or_else(malformed)?;
    Ok((old, new))
}

/// Resolve + validate the on-disk target for a section from its header paths and
/// the `-p` strip level. Returns the joined path; `Err` for unsafe/absolute.
fn resolve_target(sec: &Section, strip: Option<usize>) -> Result<PathBuf, String> {
    let raw = sec
        .new_path
        .as_deref()
        .filter(|p| *p != "/dev/null")
        .or(sec.old_path.as_deref().filter(|p| *p != "/dev/null"))
        .ok_or_else(|| "section has no usable path (both sides /dev/null)".to_string())?;
    let raw = raw.replace('\\', "/");
    if raw.starts_with('/') || raw.starts_with("//") || (raw.len() > 1 && &raw[1..2] == ":") {
        return Err(format!("refusing absolute path '{raw}'"));
    }
    let n = strip.unwrap_or(if raw.starts_with("a/") || raw.starts_with("b/") {
        1
    } else {
        0
    });
    let kept: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).skip(n).collect();
    if kept.is_empty() {
        return Err(format!("path '{raw}' fully stripped by -p{n}"));
    }
    let path = PathBuf::from(kept.join("/"));
    validate_path(&path, &raw)?;
    Ok(path)
}

fn validate_path(path: &Path, raw: &str) -> Result<(), String> {
    if path.is_absolute() || Path::new(raw).is_absolute() {
        return Err(format!("refusing absolute path '{raw}'"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("refusing path with '..' traversal: '{raw}'"));
    }
    if reject_git_internal(path).is_err() {
        return Err(format!("refusing a .git path: '{raw}'"));
    }
    Ok(())
}

/// Apply one section to its target's current bytes, producing the final image
/// (or a delete). Reads the existing file; a `/dev/null` old side starts empty.
fn apply_section(
    pinned: Option<PathBuf>,
    sec: &Section,
    strip: Option<usize>,
    verbose: u8,
) -> Result<Prepared, String> {
    let target = match pinned {
        Some(p) => p,
        None => resolve_target(sec, strip)?,
    };
    let creates = sec.old_path.as_deref() == Some("/dev/null");
    let deletes = sec.new_path.as_deref() == Some("/dev/null");

    let patch = diffy::Patch::from_str(&sec.patch).map_err(|e| format!("invalid diff: {e}"))?;

    // Base text + the doc's byte-fidelity facts (for an existing file).
    let doc = if creates || !target.exists() {
        None
    } else {
        Some(TextDoc::load(&target).map_err(|e| format!("{}: {e}", target.display()))?)
    };
    let base_lf = doc
        .as_ref()
        .map(|d| d.text.replace("\r\n", "\n"))
        .unwrap_or_default();

    let new_lf = diffy::apply(&base_lf, &patch).map_err(|e| format!("{e}"))?;
    let final_bytes = if deletes {
        None
    } else {
        let crlf = doc.as_ref().is_some_and(|d| d.crlf);
        let body = if crlf {
            new_lf.replace('\n', "\r\n")
        } else {
            new_lf.clone()
        };
        match &doc {
            Some(d) => Some(d.encode(&body)),
            None => Some(body.into_bytes()), // created file: UTF-8 LF
        }
    };

    // Receipt body (capped) + counts, from the patch's own lines.
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut budget = RECEIPT_CAP_LINES;
    let mut body = String::new();
    for hunk in patch.hunks() {
        let old: Vec<&str> = hunk
            .lines()
            .iter()
            .filter_map(|l| match l {
                diffy::Line::Delete(s) => Some(s.trim_end_matches(['\r', '\n'])),
                _ => None,
            })
            .collect();
        let new: Vec<&str> = hunk
            .lines()
            .iter()
            .filter_map(|l| match l {
                diffy::Line::Insert(s) => Some(s.trim_end_matches(['\r', '\n'])),
                _ => None,
            })
            .collect();
        added += new.len();
        removed += old.len();
        if budget > 0 {
            body.push_str(&diff_block(&old.join("\n"), &new.join("\n"), &mut budget));
        }
    }
    if verbose > 0 {
        eprintln!(
            "rtk patch: {} {} -> {} hunks (+{added}/-{removed})",
            if deletes {
                "deleting"
            } else if creates {
                "creating"
            } else {
                "patching"
            },
            target.display(),
            patch.hunks().len()
        );
    }
    Ok(Prepared {
        target,
        final_bytes,
        deletes,
        hunks: patch.hunks().len(),
        added,
        removed,
        body,
        original_bytes: doc.map(|d| d.original_bytes),
    })
}

/// Undo committed writes in reverse order: files that existed are restored from
/// their captured bytes, files that were created are removed. Best-effort —
/// individual undo failures are reported but don't stop the unwinding.
fn rollback(journal: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, original) in journal.iter().rev() {
        let r = match original {
            Some(bytes) => edit::atomic_write(path, bytes),
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            },
        };
        if let Err(e) = r {
            eprintln!("rtk patch: rollback of '{}' FAILED: {e}", path.display());
        }
    }
}

/// The write phase — only reached after EVERY section applied cleanly, so an
/// apply failure upstream never gets here. Per-file writes are atomic; each
/// pre-existing target is optionally backed up first.
fn finish(prepared: Vec<Prepared>, dry_run: bool, backup: bool, verbose: u8) -> Result<i32> {
    let mut shown = String::new();
    let noun = if prepared.len() == 1 { "file" } else { "files" };
    shown.push_str(&format!(
        "patched {} {noun}{}\n",
        prepared.len(),
        if dry_run {
            " (dry-run — nothing written)"
        } else {
            ""
        }
    ));
    for p in &prepared {
        let verb = if p.deletes {
            "deleted"
        } else if !p.target.exists() {
            "created"
        } else {
            "patched"
        };
        shown.push_str(&format!(
            "  {verb} {} ({} hunks, +{added}/-{removed})\n",
            p.target.display(),
            p.hunks,
            added = p.added,
            removed = p.removed,
        ));
        shown.push_str(&p.body);
    }

    if !dry_run {
        // Journal of committed effects, unwound in reverse if any later write
        // fails — the patch stays all-or-nothing even across IO errors.
        let mut journal: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
        for p in &prepared {
            if let Err(e) = backup_file(&p.target, backup) {
                rollback(&journal);
                eprintln!(
                    "rtk patch: {e:#} — rolled back {committed} earlier file(s)",
                    committed = journal.len()
                );
                return Ok(3);
            }
            let res = if p.deletes {
                match std::fs::remove_file(&p.target) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(anyhow::anyhow!(
                        "cannot remove '{}': {e}",
                        p.target.display()
                    )),
                }
            } else {
                edit::atomic_write(&p.target, p.final_bytes.as_deref().unwrap_or_default())
            };
            if let Err(e) = res {
                rollback(&journal);
                eprintln!(
                    "rtk patch: {e:#} — rolled back {committed} earlier file(s)",
                    committed = journal.len()
                );
                return Ok(3);
            }
            journal.push((p.target.clone(), p.original_bytes.clone()));
        }
    }
    if verbose > 0 {
        eprintln!("rtk patch: {} prepared section(s)", prepared.len());
    }

    let timer = tracking::TimedExecution::start();
    let baseline = prepared.iter().map(|p| p.added + p.removed).sum::<usize>();
    timer.track(
        &format!("patch {} file(s)", prepared.len()),
        "rtk patch",
        // baseline tokens ≈ whole-file rewrites; size the input by changed lines.
        &"x".repeat(baseline.max(1) * 40),
        &shown,
    );
    print!("{shown}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn rtk_bin() -> PathBuf {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug");
        p.push(if cfg!(windows) { "rtk.exe" } else { "rtk" });
        p
    }

    #[test]
    fn patch_roundtrip_via_diffy_directly() {
        let base = "a\nb\nc\n";
        let patch = "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let p = diffy::Patch::from_str(patch).unwrap();
        assert_eq!(diffy::apply(base, &p).unwrap(), "a\nB\nc\n");
        let p2 = diffy::Patch::from_str("@@ -1,2 +1,2 @@\n x\n-y\n+Y\n").unwrap();
        assert!(diffy::apply(base, &p2).is_err());
    }

    #[test]
    fn crlf_document_patch_preserves_style() {
        let base_crlf = "a\r\nb\r\nc\r\n";
        let text_lf = base_crlf.replace("\r\n", "\n");
        let p = diffy::Patch::from_str("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n").unwrap();
        let out = diffy::apply(&text_lf, &p).unwrap().replace('\n', "\r\n");
        assert_eq!(out, "a\r\nB\r\nc\r\n");
    }

    #[test]
    fn single_section_parses_with_paths() {
        let d = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        let secs = parse_sections(d).unwrap();
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].old_path.as_deref(), Some("a/f.txt"));
        assert_eq!(secs[0].new_path.as_deref(), Some("b/f.txt"));
        assert_eq!(
            resolve_target(&secs[0], None).unwrap(),
            PathBuf::from("f.txt")
        );
    }

    #[test]
    fn multi_section_state_machine() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n\
                 diff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-c\n+d\n";
        let secs = parse_sections(d).unwrap();
        assert_eq!(secs.len(), 2, "two git sections");
        assert_eq!(resolve_target(&secs[0], None).unwrap(), PathBuf::from("x"));
        assert_eq!(resolve_target(&secs[1], None).unwrap(), PathBuf::from("y"));
    }

    #[test]
    fn deleted_line_that_looks_like_a_header_stays_in_hunk() {
        // A context/removal line beginning "--- " must not split a section.
        let d = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n keep\n--- dramatic\n+x\n";
        let secs = parse_sections(d).unwrap();
        assert_eq!(secs.len(), 1, "no spurious new section");
        assert!(secs[0].patch.contains("--- dramatic"));
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let mk = |np: &str| Section {
            old_path: Some("a/x".into()),
            new_path: Some(np.into()),
            patch: String::new(),
        };
        assert!(resolve_target(&mk("/etc/passwd"), Some(0)).is_err());
        assert!(resolve_target(&mk("a/../../evil"), None).is_err());
        assert!(resolve_target(&mk("a/.git/config"), None).is_err());
        assert_eq!(
            resolve_target(&mk("b/sub/dir/f.rs"), None).unwrap(),
            PathBuf::from("sub/dir/f.rs")
        );
    }

    #[test]
    fn strip_levels_and_dev_null_sides() {
        // explicit -p0 keeps the full a/ path
        let s = Section {
            old_path: Some("a/f".into()),
            new_path: Some("b/f".into()),
            patch: String::new(),
        };
        assert_eq!(resolve_target(&s, Some(0)).unwrap(), PathBuf::from("b/f"));
        // creation: no old side, path from new
        let c = Section {
            old_path: Some("/dev/null".into()),
            new_path: Some("b/new.rs".into()),
            patch: String::new(),
        };
        assert_eq!(resolve_target(&c, None).unwrap(), PathBuf::from("new.rs"));
        // deletion: new is /dev/null → fall back to old
        let del = Section {
            old_path: Some("a/gone.rs".into()),
            new_path: Some("/dev/null".into()),
            patch: String::new(),
        };
        assert_eq!(
            resolve_target(&del, None).unwrap(),
            PathBuf::from("gone.rs")
        );
    }

    #[test]
    fn apply_section_builds_final_bytes_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();
        let sec = Section {
            old_path: Some("a/f.txt".into()),
            new_path: Some("b/f.txt".into()),
            patch: "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
                .into(),
        };
        // Pinned target (header paths are CWD-relative by design).
        let p = apply_section(Some(f.clone()), &sec, None, 0).unwrap();
        assert_eq!(p.target, f);
        assert_eq!(p.added, 1);
        assert_eq!(p.removed, 1);
        assert_eq!(p.final_bytes.as_deref().unwrap(), b"alpha\nBETA\ngamma\n");
        assert_eq!(
            p.original_bytes.as_deref().unwrap(),
            b"alpha\nbeta\ngamma\n"
        );
    }

    #[test]
    fn apply_section_create_and_delete_flags() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("d.txt");
        std::fs::write(&existing, "x\n").unwrap();
        let create = Section {
            old_path: Some("/dev/null".into()),
            new_path: Some("b/new.txt".into()),
            patch: "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+A\n+B\n".into(),
        };
        let pc = apply_section(Some(dir.path().join("new.txt")), &create, None, 0).unwrap();
        assert_eq!(pc.final_bytes.as_deref().unwrap(), b"A\nB\n");

        let del = Section {
            old_path: Some("a/d.txt".into()),
            new_path: Some("/dev/null".into()),
            patch: "--- a/d.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n".into(),
        };
        let pd = apply_section(Some(existing), &del, None, 0).unwrap();
        assert!(pd.deletes);
        assert!(pd.final_bytes.is_none());
    }

    #[test]
    fn git_internal_refuses_before_stdin() {
        let code = run(
            Some(PathBuf::from("repo\\.git\\config")),
            false,
            false,
            None,
            0,
        )
        .unwrap();
        assert_eq!(code, 2);
    }

    /// Spawn rtk with `args` + `cwd` (header-relative targets resolve there),
    /// feed `diff` on stdin, collect output. Requires a built debug binary
    /// (same `#[ignore]` reason as the read e2e tests).
    fn spawn_patch_in(cwd: &Path, args: &[&str], diff: &str) -> (Option<i32>, String, String) {
        let bin = rtk_bin();
        assert!(bin.exists(), "Run `cargo build` first");
        let mut child = std::process::Command::new(&bin)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn rtk patch");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(diff.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    #[ignore]
    fn multi_file_apply_and_rollback_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("y.txt"), "c\n").unwrap();
        let good = "--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\n\
                    --- a/y.txt\n+++ b/y.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let (code, stdout, stderr) = spawn_patch_in(dir.path(), &["patch"], good);
        assert_eq!(code, Some(0), "stderr: {stderr}");
        assert!(stdout.contains("patched 2 files"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
            "b\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("y.txt")).unwrap(),
            "d\n"
        );

        // Second section mismatches → all-or-nothing: x.txt must stay as-is now.
        let bad = "--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-b\n+z\n\
                   --- a/y.txt\n+++ b/y.txt\n@@ -1 +1 @@\nNOPE\n+q\n";
        let (code, _, _stderr) = spawn_patch_in(dir.path(), &["patch"], bad);
        assert_eq!(code, Some(1), "mismatched section must fail");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt")).unwrap(),
            "b\n",
            "nothing rewritten on failure"
        );
    }

    #[test]
    #[ignore]
    fn single_file_dry_run_backup_and_apply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";

        let (code, stdout, _) =
            spawn_patch_in(dir.path(), &["patch", file.to_str().unwrap()], diff);
        assert_eq!(code, Some(0));
        assert!(stdout.contains("patched 1 file"), "stdout: {stdout}");
        assert!(stdout.contains("1 hunks, +1/-1"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\ngamma\n"
        );

        // --backup: a .bak with the pre-edit bytes sits beside the file.
        let (code, _, err) = spawn_patch_in(
            dir.path(),
            &["patch", "--backup", file.to_str().unwrap()],
            "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n alpha\n BETA\n-gamma\n+GAMMA\n",
        );
        assert_eq!(code, Some(0), "stderr: {err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt.bak")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\nGAMMA\n"
        );
    }
}
