//! `rtk patch` — apply a unified diff to one file, all-or-nothing, with a
//! cheap receipt. The multi-edit counterpart of `rtk edit`: where an agent
//! knows several regions change, one patch costs fewer tokens than several
//! find/replace rounds.
//!
//! Scope (file-editing spec P2): a single target file passed on the command
//! line, the diff on stdin — `rtk patch <file> < changes.diff`. Multi-file
//! git-format patches are P3 and refused explicitly. `--dry-run` rehearses
//! with zero writes. Fidelity matches `rtk edit`: the diff's LF lines are
//! translated to the document's own newline style, the file stays in its
//! encoding, and any failed hunk means nothing was written.

use crate::core::edit::{self, RECEIPT_CAP_LINES, TextDoc, diff_block, reject_git_internal};
use crate::core::tracking;
use anyhow::Result;
use std::io::Read;
use std::path::PathBuf;

/// Exit codes shared with `rtk edit`: 0 ok · 1 a hunk failed to apply (nothing
/// written) · 2 usage/parse/refusal · 3 IO failure.
pub fn run(file: PathBuf, dry_run: bool, verbose: u8) -> Result<i32> {
    if let Err(e) = reject_git_internal(&file) {
        eprintln!("rtk patch: {e}");
        return Ok(2);
    }
    let doc = match TextDoc::load(&file) {
        Ok(d) => d,
        Err(e) => {
            let is_io = e.root_cause().downcast_ref::<std::io::Error>().is_some();
            eprintln!("rtk patch: {:#}", e);
            return Ok(if is_io { 3 } else { 2 });
        }
    };
    let mut patch_in = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut patch_in) {
        eprintln!("rtk patch: cannot read stdin: {e}");
        return Ok(2);
    }
    if patch_in.trim().is_empty() {
        eprintln!("rtk patch: no unified diff on stdin (usage: rtk patch <file> < changes.diff)");
        return Ok(2);
    }
    // diffy parses one file's hunks; git multi-file patches stack repeated
    // `--- ` headers. Fail loudly instead of applying a prefix silently.
    if patch_in.matches("\n--- ").count() + usize::from(patch_in.starts_with("--- ")) > 1 {
        eprintln!("rtk patch: multi-file patches are not supported yet — split by file");
        return Ok(2);
    }

    // The diff protocol is LF; normalize both sides so context lines match a
    // CRLF document, and restore the document's style afterwards.
    let text_lf = doc.text.replace("\r\n", "\n");
    let patch_lf = patch_in.replace("\r\n", "\n");
    let patch = match diffy::Patch::from_str(&patch_lf) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rtk patch: invalid unified diff: {e}");
            return Ok(2);
        }
    };
    let new_text_lf = match diffy::apply(&text_lf, &patch) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("rtk patch: {e} — nothing written");
            return Ok(1);
        }
    };
    let final_text = if doc.crlf {
        new_text_lf.replace('\n', "\r\n")
    } else {
        new_text_lf
    };

    // Receipt: header counts + a capped block per hunk.
    //
    // NB: diffy's Line content INCLUDES the line's own newline; trim it or the
    // join-then-split in `diff_block` emits a phantom empty `-`/`+` line per
    // hunk side.
    let hunks = patch.hunks();
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut body = String::new();
    let mut budget = RECEIPT_CAP_LINES;
    for hunk in hunks.iter() {
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
    let mut shown = format!(
        "{} hunks applied to {} (+{added}/-{removed}){}\n",
        hunks.len(),
        file.display(),
        if dry_run {
            " (dry-run — nothing written)"
        } else {
            ""
        }
    );
    shown.push_str(&body);
    if hunks.len() > 1 && budget == 0 {
        shown.push_str("… (further hunks not shown)\n");
    }

    if !dry_run && let Err(e) = edit::atomic_write(&file, &doc.encode(&final_text)) {
        eprintln!("rtk patch: {:#}", e);
        return Ok(3);
    }
    if verbose > 0 {
        eprintln!("rtk patch: encoding {:?}, crlf={}", doc.encoding, doc.crlf);
    }
    let timer = tracking::TimedExecution::start();
    timer.track(
        &format!("rewrite whole file {}", file.display()),
        "rtk patch",
        &doc.text,
        &shown,
    );
    print!("{shown}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Real-binary path (needs `cargo build` first — same `#[ignore]` contract
    /// as the read e2e tests, since `cargo test` only builds the test harness).
    fn rtk_bin() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
        // A hunk whose context no longer matches must fail, not half-apply.
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
    fn git_internal_refuses_before_stdin() {
        // Must not hang reading stdin: refusal happens first.
        let code = run(PathBuf::from("repo\\.git\\config"), false, 0).unwrap();
        assert_eq!(code, 2);
    }

    /// Spawn rtk with `args`, feed `diff` on stdin, collect output. Requires a
    /// built debug binary (same `#[ignore]` reason as the read e2e tests).
    fn spawn_patch(args: &[&str], diff: &str) -> (Option<i32>, String, String) {
        let bin = rtk_bin();
        assert!(bin.exists(), "Run `cargo build` first");
        let mut child = std::process::Command::new(&bin)
            .args(args)
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
    fn multi_file_patch_refused_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, "a\n").unwrap();
        let diff = "--- a/x.txt\n+++ b/x.txt\n@@ -1 +1 @@\n-a\n+b\n\
                    --- a/y.txt\n+++ b/y.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let (code, _stdout, stderr) = spawn_patch(&["patch", file.to_str().unwrap()], diff);
        assert_eq!(code, Some(2), "multi-file must be refused");
        assert!(stderr.contains("multi-file"), "stderr: {stderr}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "a\n",
            "refusal writes nothing"
        );
    }

    #[test]
    #[ignore]
    fn dry_run_then_apply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";

        // --dry-run: receipt says dry-run, file untouched.
        let (code, stdout, _stderr) =
            spawn_patch(&["patch", "--dry-run", file.to_str().unwrap()], diff);
        assert_eq!(code, Some(0));
        assert!(stdout.contains("dry-run"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        // real apply: bytes change, receipt counts the hunk.
        let (code, stdout, _stderr) = spawn_patch(&["patch", file.to_str().unwrap()], diff);
        assert_eq!(code, Some(0));
        assert!(stdout.contains("1 hunks applied"), "stdout: {stdout}");
        assert!(stdout.contains("(+1/-1)"), "stdout: {stdout}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }
}
