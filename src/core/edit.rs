//! Shared write-side kernel for `rtk edit` / `rtk patch`.
//!
//! Owns the three promises every rtk write path must keep:
//! 1. **Byte fidelity** — untouched bytes come back unchanged: CRLF style is
//!    preserved, a UTF-8 BOM stays, a GBK file stays GBK.
//! 2. **All-or-nothing** — the file is only ever replaced by an atomic
//!    temp-file + rename after the whole edit succeeded; a rejected edit
//!    writes zero bytes.
//! 3. **Cheap receipts** — success is one line plus a small `-`/`+` block,
//!    never the whole file.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;

/// Cap for the receipt body: lines beyond this collapse to a `… (+N lines)`
/// note. The header line is never capped.
pub const RECEIPT_CAP_LINES: usize = 20;

/// In-document encodings rtk will round-trip. Anything else is refused: a
/// wrong guess at decode time is silent corruption at write time.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DocEncoding {
    Utf8,
    Utf8Bom,
    Gbk,
}

/// A text file loaded for editing: decoded view, original bytes, and the
/// encoding/newline facts needed to write it back faithfully.
pub struct TextDoc {
    pub text: String,
    pub encoding: DocEncoding,
    /// The file uses CRLF line endings (majority wins; mixed files keep their
    /// untouched regions verbatim either way).
    pub crlf: bool,
    /// Exact bytes on disk, kept so a multi-file transaction can restore a
    /// file byte-for-byte during unwinding without trusting the decode/encode
    /// round-trip.
    pub original_bytes: Vec<u8>,
}

impl TextDoc {
    /// Load `path`, rejecting binaries and undecodable encodings before any
    /// edit logic runs. NUL in the first 8 KiB is the binary test; decoding is
    /// strict UTF-8, then strict GBK (cp936) — the two things an ANSI-era
    /// Windows codebase actually contains.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs_read(path)?;
        Self::from_bytes(&bytes).with_context(|| format!("{}", path.display()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes[..bytes.len().min(8192)].contains(&0) {
            bail!("binary file (NUL byte in first 8 KiB) — refusing to edit");
        }
        let crlf = {
            let cr = bytes.iter().filter(|&&b| b == b'\r').count();
            let lf = bytes.iter().filter(|&&b| b == b'\n').count();
            lf > 0 && cr * 2 >= lf
        };
        let without_bom = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
        let (encoding, text) = match std::str::from_utf8(without_bom) {
            Ok(s) => {
                let enc = if bytes.len() != without_bom.len() {
                    DocEncoding::Utf8Bom
                } else {
                    DocEncoding::Utf8
                };
                (enc, s.to_string())
            }
            Err(_) => {
                let (cow, _, had_errors) = encoding_rs::GBK.decode(without_bom);
                if had_errors {
                    bail!("not valid UTF-8 or GBK — refusing to edit an undecodable file");
                }
                (DocEncoding::Gbk, cow.into_owned())
            }
        };
        Ok(Self {
            text,
            encoding,
            crlf,
            original_bytes: bytes.to_vec(),
        })
    }

    /// Re-encode an edited string to the document's original byte shape.
    pub fn encode(&self, text: &str) -> Vec<u8> {
        let body = match self.encoding {
            DocEncoding::Gbk => encoding_rs::GBK.encode(text).0.into_owned(),
            DocEncoding::Utf8 | DocEncoding::Utf8Bom => text.as_bytes().to_vec(),
        };
        match self.encoding {
            DocEncoding::Utf8Bom => {
                let mut out = Vec::with_capacity(3 + body.len());
                out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
                out.extend_from_slice(&body);
                out
            }
            _ => body,
        }
    }
}

fn fs_read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read '{}'", path.display()))
}

/// On a CRLF document, a multi-line `find` (and its `replace`) authored
/// against the normalized `\n` view is rewritten to `\r\n` so the edit hits
/// and the file stays line-ending-homogeneous. LF documents (and needles that
/// already contain `\r\n`) pass through untouched.
pub fn crlf_adapt(text: &str, find: &str, replace: &str) -> (String, String) {
    if text.contains("\r\n") && find.contains('\n') && !find.contains("\r\n") {
        let find = find.replace('\n', "\r\n");
        let replace = if replace.contains('\n') && !replace.contains("\r\n") {
            replace.replace('\n', "\r\n")
        } else {
            replace.to_string()
        };
        (find, replace)
    } else {
        (find.to_string(), replace.to_string())
    }
}

/// All non-overlapping spans of a verbatim `find`, after [`crlf_adapt`].
pub fn find_matches(text: &str, find: &str) -> Vec<(usize, usize)> {
    let (needle, _) = crlf_adapt(text, find, "");
    if needle.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(&needle) {
        let abs = cursor + rel;
        spans.push((abs, abs + needle.len()));
        cursor = abs + needle.len();
    }
    spans
}

/// Splice sorted, non-overlapping `(start, end, replacement)` changes into
/// `text` (the replacement is already in the document's newline shape).
pub fn splice(text: &str, changes: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end, replace) in changes {
        out.push_str(&text[cursor..*start]);
        out.push_str(replace);
        cursor = *end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// 1-based line number containing byte offset `at`.
pub fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

/// Render the standard success receipt: `N replacements @ line(s) …` plus a
/// capped `-`/`+` block per change. `old`/`new` slices pair up per change.
pub fn receipt(changes: &[(String, String)], line_numbers: &[usize], preview: bool) -> String {
    let mut out = String::new();
    let n = changes.len();
    let lines = if line_numbers.len() == 1 {
        format!("line {}", line_numbers[0])
    } else {
        format!(
            "lines {}",
            line_numbers
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    out.push_str(&format!(
        "{} {lines}{}\n",
        if n == 1 {
            "1 replacement @".to_string()
        } else {
            format!("{n} replacements @")
        },
        if preview {
            " (preview — nothing written)"
        } else {
            ""
        }
    ));
    let mut budget = RECEIPT_CAP_LINES;
    for (idx, (old, new)) in changes.iter().enumerate() {
        if budget == 0 {
            out.push_str(&format!("… ({} more changes not shown)\n", n - idx));
            break;
        }
        let block = diff_block(old, new, &mut budget);
        out.push_str(&block);
    }
    out
}

/// One `-old/+new` block. Each side's lines are counted against the shared
/// `budget`; overflow collapses to a `… (+N lines)` note rather than a
/// truncated half-line.
pub fn diff_block(old: &str, new: &str, budget: &mut usize) -> String {
    let mut out = String::new();
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let per_side = (*budget).clamp(1, 20);
    let shown_old = old_lines.len().min(per_side);
    let shown_new = new_lines.len().min(per_side);
    for l in &old_lines[..shown_old] {
        out.push_str(&format!("-{l}\n"));
    }
    for l in &new_lines[..shown_new] {
        out.push_str(&format!("+{l}\n"));
    }
    let hidden = (old_lines.len() - shown_old) + (new_lines.len() - shown_new);
    if hidden > 0 {
        out.push_str(&format!("… (+{hidden} lines not shown)\n"));
    }
    *budget = budget.saturating_sub(shown_old + shown_new);
    out
}

/// Atomic replacement: write `bytes` to a sibling temp file, flush to disk,
/// then rename over `path`. A crash mid-write leaves the original untouched;
/// rename replaces atomically on Windows (MoveFileEx REPLACE_EXISTING) and
/// POSIX alike.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rtk-edit".to_string());
    let tmp = parent.join(format!(".{name}.rtk-tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot create '{}'", tmp.display()))?;
        f.write_all(bytes)?;
        f.flush()?;
        drop(f);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("cannot replace '{}'", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Copy `path` to `<path>.bak` before a write, when `--backup` was requested
/// and the file exists (a created file has nothing to back up). Returns the
/// backup path so callers can report it. An existing `.bak` is replaced —
/// `--backup` means "keep the version about to be overwritten", not an
/// archive; the atomic-write contract means a `.bak` is only ever written
/// from a verified-good read of the original.
pub fn backup_file(path: &Path, enabled: bool) -> Result<Option<std::path::PathBuf>> {
    if !enabled || !path.exists() {
        return Ok(None);
    }
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rtk-backup".to_string());
    name.push_str(".bak");
    let bak = path.with_file_name(name);
    std::fs::copy(path, &bak)
        .with_context(|| format!("cannot back up '{}' to '{}'", path.display(), bak.display()))?;
    Ok(Some(bak))
}

/// Refuse to touch files inside a `.git` directory — rtk edits are for
/// working files, not repository internals.
pub fn reject_git_internal(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(s) if s.eq_ignore_ascii_case(".git")))
    {
        bail!("refusing to edit inside a .git directory");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textdoc_roundtrips_lf_crlf_bom() {
        let lf = TextDoc::from_bytes(b"a\nb\n").unwrap();
        assert_eq!(lf.encoding, DocEncoding::Utf8);
        assert!(!lf.crlf);
        assert_eq!(lf.encode("x\ny\n"), b"x\ny\n");

        let crlf = TextDoc::from_bytes(b"a\r\nb\r\n").unwrap();
        assert!(crlf.crlf);

        let bom = TextDoc::from_bytes("\u{feff}a\n".as_bytes()).unwrap();
        assert_eq!(bom.encoding, DocEncoding::Utf8Bom);
        assert_eq!(
            bom.encode("b\n"),
            [&[0xEF, 0xBB, 0xBF][..], b"b\n"].concat()
        );
    }

    #[test]
    fn textdoc_gbk_and_rejections() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("测试 内容\n");
        let doc = TextDoc::from_bytes(&gbk_bytes).unwrap();
        assert_eq!(doc.encoding, DocEncoding::Gbk);
        assert!(doc.text.contains("测试"));
        // Re-encoding preserves the codepage.
        let back = doc.encode(&doc.text.replace("测试", "测试2"));
        let (decoded, _, _) = encoding_rs::GBK.decode(&back);
        assert!(decoded.contains("测试2"));

        // Binary rejection: NUL early.
        assert!(TextDoc::from_bytes(&[0x00, 0x01, 0x02]).is_err());
        // Undecodable garbage: invalid in both UTF-8 and GBK strict.
        assert!(TextDoc::from_bytes(&[0xff, 0xfe, 0x80, 0x81, 0x82, 0xff, 0xfe]).is_err());
    }

    #[test]
    fn lf_authored_find_matches_and_splices_on_crlf_text() {
        let text = "fn a() {\r\n    x();\r\n}\r\n";
        let spans = find_matches(text, "    x();\n");
        assert_eq!(spans.len(), 1);
        let (_, piece) = crlf_adapt(text, "    x();\n", "    y();\n");
        let out = splice(text, &[(spans[0].0, spans[0].1, piece)]);
        // The replacement landed with CRLF endings: file stays homogeneous.
        assert_eq!(out, "fn a() {\r\n    y();\r\n}\r\n");
    }

    #[test]
    fn find_matches_counts_multiple_and_offsets() {
        let offs = find_matches("a X b X c", "X");
        assert_eq!(offs, vec![(2, 3), (6, 7)]);
        let (_, piece) = crlf_adapt("a X b X c", "X", "Y");
        assert_eq!(
            splice("a X b X c", &[(2, 3, piece.clone()), (6, 7, piece)]),
            "a Y b Y c"
        );
    }

    #[test]
    fn line_of_counts_newlines() {
        assert_eq!(line_of("a\nb\nc", 0), 1);
        assert_eq!(line_of("a\nb\nc", 2), 2);
        assert_eq!(line_of("a\nb\nc", 4), 3);
    }

    #[test]
    fn receipt_shapes() {
        let r = receipt(&[("fn main(".into(), "pub fn main(".into())], &[42], false);
        assert!(r.starts_with("1 replacement @ line 42\n"));
        assert!(r.contains("-fn main(\n+pub fn main(\n"));
        assert!(!r.contains("preview"));

        let multi = receipt(
            &[("a".into(), "b".into()), ("c".into(), "d".into())],
            &[1, 9],
            true,
        );
        assert!(multi.starts_with("2 replacements @ lines 1, 9 (preview — nothing written)\n"));

        // Budget: huge changes collapse with a note, never a crash.
        let big_old = "x\n".repeat(50);
        let mut budget = 20usize;
        let block = diff_block(&big_old, "y", &mut budget);
        assert!(block.contains("lines not shown"));
        assert!(budget == 0);
    }

    #[test]
    fn atomic_write_replaces_and_rejects_git_paths() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("target.txt");
        std::fs::write(&f, b"old").unwrap();
        atomic_write(&f, b"new").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
        // No temp litter left behind on success.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");

        assert!(reject_git_internal(Path::new("repo/.git/config")).is_err());
        assert!(reject_git_internal(Path::new("repo/src/main.rs")).is_ok());
    }

    #[test]
    fn backup_file_naming_and_gating() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("file.txt");
        let plain = dir.path().join("Makefile");
        std::fs::write(&txt, "one").unwrap();
        std::fs::write(&plain, "two").unwrap();

        // Disabled → no-op, no file.
        assert_eq!(backup_file(&txt, false).unwrap(), None);
        assert!(!dir.path().join("file.txt.bak").exists());

        // Enabled → sibling .bak with the original bytes; extensionless names
        // still get `.bak` appended (never `..bak` or a replaced extension).
        assert_eq!(
            backup_file(&txt, true).unwrap().unwrap(),
            dir.path().join("file.txt.bak")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt.bak")).unwrap(),
            "one"
        );
        assert!(backup_file(&plain, true).unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Makefile.bak")).unwrap(),
            "two"
        );

        // Missing source (a to-be-created file) backs up nothing.
        assert_eq!(
            backup_file(&dir.path().join("ghost.rs"), true).unwrap(),
            None
        );
    }
}
