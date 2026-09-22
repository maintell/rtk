//! `rtk edit` — explicit, receipt-cheap single-match file editing.
//!
//! The agent's token cost of an edit should be the edit, not the file:
//! `--find`/`--replace` in, one receipt line plus a small `-`/`+` block out.
//! Deliberately strict (see the file-editing spec): the default requires
//! EXACTLY ONE match; zero or many write nothing and exit 1. `--all` opts into
//! replacing every match; `--range A-B` narrows consideration to a line window.
//! `rtk edit` never touches repository internals and refuses binaries and
//! undecodable files up front.

use crate::core::edit::{
    self, TextDoc, find_matches, line_of, receipt, reject_git_internal, splice,
};
use crate::core::tracking;
use anyhow::Result;
use std::path::PathBuf;

/// Everything `rtk edit` needs, parsed loosely by clap (usage errors are
/// produced here with exit 2 — clap errors on non-meta commands get swallowed
/// by run_fallback and would degrade into "exec edit").
pub struct EditRequest {
    pub file: PathBuf,
    pub find: Option<String>,
    pub replace: Option<String>,
    pub all: bool,
    pub regex: bool,
    pub preview: bool,
    pub range: Option<String>,
}

/// Exit codes (spec §5): 0 ok · 1 match failure (zero bytes written) ·
/// 2 usage/refusal · 3 IO failure.
pub fn run(req: EditRequest, verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let file = req.file;

    // ---- usage / refusals (exit 2) ----
    let Some(find) = req.find.filter(|f| !f.is_empty()) else {
        eprintln!("rtk edit: --find is required and must be non-empty");
        return Ok(2);
    };
    let replace = req.replace.clone().unwrap_or_default();
    if let Err(e) = reject_git_internal(&file) {
        eprintln!("rtk edit: {e}");
        return Ok(2);
    }
    let range = match req
        .range
        .as_deref()
        .map(crate::cmds::system::read::parse_line_range)
    {
        None => None,
        Some(Ok(r)) => Some(r),
        Some(Err(msg)) => {
            eprintln!("rtk edit: --range: {msg}");
            return Ok(2);
        }
    };

    // ---- load (refusal exit 2 / IO exit 3) ----
    let doc = match TextDoc::load(&file) {
        Ok(d) => d,
        Err(e) => {
            let is_io = e.root_cause().downcast_ref::<std::io::Error>().is_some();
            eprintln!("rtk edit: {:#}", e);
            return Ok(if is_io { 3 } else { 2 });
        }
    };
    if verbose > 0 {
        eprintln!(
            "rtk edit: {} bytes, encoding {:?}, crlf={}",
            doc.text.len(),
            doc.encoding,
            doc.crlf
        );
    }

    // ---- match discovery: spans paired with their (newline-adapted) piece ----
    let mut changes: Vec<(usize, usize, String)> = if req.regex {
        let re = match regex::Regex::new(&find) {
            Ok(re) => re,
            Err(e) => {
                eprintln!("rtk edit: invalid --regex: {e}");
                return Ok(2);
            }
        };
        re.captures_iter(&doc.text)
            .filter_map(|caps| {
                let whole = caps.get(0)?;
                let mut piece = String::new();
                caps.expand(&replace, &mut piece);
                Some((whole.start(), whole.end(), piece))
            })
            .collect()
    } else {
        let (needle, piece) = edit::crlf_adapt(&doc.text, &find, &replace);
        if needle.is_empty() {
            eprintln!("rtk edit: --find is required and must be non-empty");
            return Ok(2);
        }
        find_matches(&doc.text, &find)
            .into_iter()
            .map(|(s, e)| (s, e, piece.clone()))
            .collect()
    };
    if let Some((first, last)) = range {
        changes.retain(|(s, _, _)| {
            let line = line_of(&doc.text, *s);
            line >= first && line <= last
        });
    }

    // ---- enforcement (exit 1, zero bytes) ----
    match changes.len() {
        0 => {
            eprintln!(
                "rtk edit: no match{} in {}",
                if range.is_some() {
                    " within --range"
                } else {
                    ""
                },
                file.display()
            );
            return Ok(1);
        }
        n if n > 1 && !req.all => {
            let lines: Vec<usize> = changes
                .iter()
                .map(|(s, _, _)| line_of(&doc.text, *s))
                .collect();
            eprintln!(
                "rtk edit: {n} matches (lines {}) — pass --all to replace every one, \
                 or narrow with --range / a longer --find",
                lines
                    .iter()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(1);
        }
        _ => {}
    }

    // ---- apply + receipt ----
    let new_text = splice(&doc.text, &changes);
    let lines: Vec<usize> = changes
        .iter()
        .map(|(s, _, _)| line_of(&doc.text, *s))
        .collect();
    let blocks: Vec<(String, String)> = changes
        .iter()
        .map(|(s, e, piece)| (doc.text[*s..*e].to_string(), piece.clone()))
        .collect();
    let shown = receipt(&blocks, &lines, req.preview);

    if !req.preview
        && let Err(e) = edit::atomic_write(&file, &doc.encode(&new_text))
    {
        eprintln!("rtk edit: {:#}", e);
        return Ok(3);
    }
    print!("{shown}");
    timer.track(
        &format!("rewrite whole file {}", file.display()),
        "rtk edit",
        &doc.text,
        &shown,
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_file(bytes: &[u8], suffix: &str) -> NamedTempFile {
        let mut f = NamedTempFile::with_suffix(suffix).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn req(file: &std::path::Path) -> EditRequest {
        EditRequest {
            file: file.to_path_buf(),
            find: None,
            replace: None,
            all: false,
            regex: false,
            preview: false,
            range: None,
        }
    }

    #[test]
    fn unique_replace_writes_and_reports_receipt() {
        let f = write_file(b"line one\nline two fn main() here\nline three\n", ".rs");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some("fn main()".into()),
                replace: Some("pub fn main()".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line one\nline two pub fn main() here\nline three\n"
        );
    }

    #[test]
    fn zero_matches_writes_nothing_and_exits_1() {
        let f = write_file(b"nothing here\n", ".txt");
        let path = f.path().to_path_buf();
        let before = std::fs::read(&path).unwrap();
        let code = run(
            EditRequest {
                find: Some("absent".into()),
                replace: Some("x".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn multiple_matches_need_all_or_range_and_name_the_lines() {
        let f = write_file(b"x X y\nz X w\n", ".txt");
        let path = f.path().to_path_buf();
        let before = std::fs::read(&path).unwrap();
        let code = run(
            EditRequest {
                find: Some("X".into()),
                replace: Some("Y".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 1, "ambiguous edit must refuse");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // --all replaces both.
        let code = run(
            EditRequest {
                find: Some("X".into()),
                replace: Some("Y".into()),
                all: true,
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x Y y\nz Y w\n");
    }

    #[test]
    fn range_disambiguates_two_matches() {
        let f = write_file(b"x X y\nz X w\n", ".txt");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some("X".into()),
                replace: Some("Y".into()),
                range: Some("2-2".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x X y\nz Y w\n");
    }

    #[test]
    fn preview_writes_nothing() {
        let f = write_file(b"a TODO b\n", ".txt");
        let path = f.path().to_path_buf();
        let before = std::fs::read(&path).unwrap();
        let code = run(
            EditRequest {
                find: Some("TODO".into()),
                replace: Some("DONE".into()),
                preview: true,
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn crlf_multiline_find_preserves_style() {
        let f = write_file(b"fn a() {\r\n    x();\r\n}\r\n", ".rs");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some("    x();\n".into()), // LF-authored against CRLF file
                replace: Some("    y();\n".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn a() {\r\n    y();\r\n}\r\n"
        );
    }

    #[test]
    fn regex_mode_expands_groups_and_stays_guarded() {
        let f = write_file(b"version: 42\n", ".txt");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some(r"version: (\d+)".into()),
                replace: Some("version = $1".into()),
                regex: true,
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = 42\n");

        // Invalid regex → usage error 2.
        let code = run(
            EditRequest {
                find: Some("(".into()),
                replace: Some("x".into()),
                regex: true,
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn refuses_binary_git_and_missing_find() {
        let f = write_file(&[0x00u8, 0x01, b'x', 0x00], ".bin");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some("x".into()),
                replace: None,
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 2, "binary refused");

        // .git-internal refusal happens even before reading.
        let r = run(
            EditRequest {
                find: Some("x".into()),
                ..req(&PathBuf::from("repo/.git/config"))
            },
            0,
        );
        assert_eq!(r.unwrap(), 2);

        // Missing --find → 2.
        let code = run(req(&path), 0).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn gbk_file_stays_gbk_after_edit() {
        let (gbk, _, _) = encoding_rs::GBK.encode("标题 说明\n");
        let f = write_file(&gbk, ".txt");
        let path = f.path().to_path_buf();
        let code = run(
            EditRequest {
                find: Some("标题".into()),
                replace: Some("题目".into()),
                ..req(&path)
            },
            0,
        )
        .unwrap();
        assert_eq!(code, 0);
        let bytes = std::fs::read(&path).unwrap();
        let (decoded, _, had_errors) = encoding_rs::GBK.decode(&bytes);
        assert!(!had_errors);
        assert!(decoded.contains("题目"));
    }
}
