/// Compact filter for `wc` — strips redundant paths and alignment padding.
///
/// Compression examples:
/// - `wc file.py`     → `30L 96W 978B`
/// - `wc -l file.py`  → `30`
/// - `wc -w file.py`  → `96`
/// - `wc -c file.py`  → `978`
/// - `wc -l *.py`     → table with common path prefix stripped
use crate::core::runner::{self, RunOptions};
use crate::core::utils::{ChildArgExt, resolved_command};
use anyhow::Result;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    // Windows has no `wc`; count the operands (or stdin) natively and reuse the
    // exact output filter so the compact format matches the Unix path.
    if cfg!(windows) {
        return run_native(args, verbose);
    }

    let mut cmd = resolved_command("wc");
    cmd.child_args(args);

    if verbose > 0 {
        eprintln!("Running: wc {}", args.join(" "));
    }

    let mode = detect_mode(args);

    // No file operands → wc reads from stdin. Forward rtk's stdin to the child
    // so `cat file | rtk wc` counts the piped data instead of reporting zero.
    let reads_stdin = !args.iter().any(|a| !a.starts_with('-'));
    let opts = if reads_stdin {
        RunOptions::stdout_only().inherit_stdin()
    } else {
        RunOptions::stdout_only()
    };

    runner::run_filtered(
        cmd,
        "wc",
        &args.join(" "),
        |stdout| filter_wc_output(stdout, &mode),
        opts,
    )
}

/// Which columns the user requested
#[derive(Debug, PartialEq)]
enum WcMode {
    /// Default: lines, words, bytes (3 columns)
    Full,
    /// Lines only (-l)
    Lines,
    /// Words only (-w)
    Words,
    /// Bytes only (-c)
    Bytes,
    /// Chars only (-m)
    Chars,
    /// Multiple flags combined — keep compact format
    Mixed,
}

/// One operand's counts, read from a file. Returns `None` (after printing the
/// error) when the file can't be read, mirroring `wc` skipping unreadable args.
fn count_file(path: &str) -> Option<Counts> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("wc: {path}: {e}");
            return None;
        }
    };
    Some(count_bytes(&bytes))
}

/// Which of lines / words / chars / bytes `wc` will print, in its fixed column
/// order (newlines, words, characters, bytes). No count flag → the default
/// triple (lines, words, bytes).
fn selected_columns(args: &[String]) -> (bool, bool, bool, bool) {
    let (mut l, mut w, mut m, mut c) = (false, false, false, false);
    for a in args.iter().filter(|a| a.starts_with('-')) {
        for ch in a.chars().skip(1) {
            match ch {
                'l' => l = true,
                'w' => w = true,
                'm' => m = true,
                'c' => c = true,
                _ => {}
            }
        }
    }
    if !l && !w && !m && !c {
        (true, true, false, true) // default: lines words bytes
    } else {
        (l, w, m, c)
    }
}

/// Synthesize a `wc`-style line (fixed-width numbers, right-aligned by 7) so the
/// shared `filter_wc_output` can reformat it identically to the Unix path.
fn emit_wc_line(name: Option<&str>, ct: &Counts, sel: (bool, bool, bool, bool)) -> String {
    let (l, w, m, c) = sel;
    let mut nums: Vec<String> = Vec::new();
    if l {
        nums.push(ct.lines.to_string());
    }
    if w {
        nums.push(ct.words.to_string());
    }
    if m {
        nums.push(ct.chars.to_string());
    }
    if c {
        nums.push(ct.bytes.to_string());
    }
    let padded: Vec<String> = nums.iter().map(|n| format!("{:>7}", n)).collect();
    match name {
        Some(n) => format!("{} {}", padded.join(" "), n),
        None => padded.join(" "),
    }
}

fn run_native(args: &[String], verbose: u8) -> Result<i32> {
    let mode = detect_mode(args);
    let sel = selected_columns(args);
    let operands: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect();
    let counts: Vec<(String, Counts)> = if operands.is_empty() {
        // stdin mode: count the piped bytes once, no filename column.
        let mut buf = Vec::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_end(&mut buf);
        vec![("(stdin)".to_string(), count_bytes(&buf))]
    } else {
        operands
            .iter()
            .filter_map(|p| count_file(p).map(|c| (p.clone(), c)))
            .collect()
    };

    let total = counts.iter().fold(Counts::default(), |mut a, (_, b)| {
        a.lines += b.lines;
        a.words += b.words;
        a.bytes += b.bytes;
        a.chars += b.chars;
        a
    });

    let mut raw = String::new();
    if counts.is_empty() {
        // every operand unreadable
        print!("{}", filter_wc_output("", &mode));
        return Ok(1);
    } else if operands.is_empty() {
        // stdin: single line, no name, no "total"
        raw.push_str(&emit_wc_line(None, &counts[0].1, sel));
        raw.push('\n');
    } else {
        for (name, ct) in &counts {
            raw.push_str(&emit_wc_line(Some(name.as_str()), ct, sel));
            raw.push('\n');
        }
        if counts.len() > 1 {
            raw.push_str(&emit_wc_line(Some("total"), &total, sel));
            raw.push('\n');
        }
    }

    if verbose > 0 {
        eprintln!("rtk wc: {} operand(s) (native)", counts.len());
    }
    print!("{}", filter_wc_output(&raw, &mode));
    Ok(0)
}

/// A file's `wc` counts: lines, words, bytes, characters.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    lines: usize,
    words: usize,
    bytes: usize,
    chars: usize,
}

/// Count bytes the way `wc` does. Words = maximal runs of non-whitespace;
/// lines = `\n` count (a final unterminated line is not counted, matching `wc`);
/// chars = UTF-8 code points.
fn count_bytes(bytes: &[u8]) -> Counts {
    let lines = bytes.iter().filter(|&&b| b == b'\n').count();
    let words = bytes
        .split(|b| (*b as char).is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .count();
    let text = String::from_utf8_lossy(bytes);
    Counts {
        lines,
        words,
        bytes: bytes.len(),
        chars: text.chars().count(),
    }
}

fn detect_mode(args: &[String]) -> WcMode {
    let flags: Vec<&str> = args
        .iter()
        .filter(|a| a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();

    if flags.is_empty() {
        return WcMode::Full;
    }

    // Collect all single-char flags (handles combined flags like -lw)
    let mut has_l = false;
    let mut has_w = false;
    let mut has_c = false;
    let mut has_m = false;
    let mut flag_count = 0;

    for flag in &flags {
        for ch in flag.chars().skip(1) {
            match ch {
                'l' => {
                    has_l = true;
                    flag_count += 1;
                }
                'w' => {
                    has_w = true;
                    flag_count += 1;
                }
                'c' => {
                    has_c = true;
                    flag_count += 1;
                }
                'm' => {
                    has_m = true;
                    flag_count += 1;
                }
                _ => {}
            }
        }
    }

    if flag_count == 0 {
        return WcMode::Full;
    }
    if flag_count > 1 {
        return WcMode::Mixed;
    }

    if has_l {
        WcMode::Lines
    } else if has_w {
        WcMode::Words
    } else if has_c {
        WcMode::Bytes
    } else if has_m {
        WcMode::Chars
    } else {
        WcMode::Full
    }
}

fn filter_wc_output(raw: &str, mode: &WcMode) -> String {
    let lines: Vec<&str> = raw.trim().lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    // Single file (one output line, no "total")
    if lines.len() == 1 {
        return format_single_line(lines[0], mode);
    }

    // Multiple files — compact table
    format_multi_line(&lines, mode)
}

/// Format a single wc output line (one file or stdin)
fn format_single_line(line: &str, mode: &WcMode) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();

    match mode {
        WcMode::Lines | WcMode::Words | WcMode::Bytes | WcMode::Chars => {
            // First number is the only requested column
            parts.first().map(|s| s.to_string()).unwrap_or_default()
        }
        WcMode::Full => {
            if parts.len() >= 3 {
                format!("{}L {}W {}B", parts[0], parts[1], parts[2])
            } else {
                line.trim().to_string()
            }
        }
        WcMode::Mixed => {
            // Strip file path, keep numbers only
            if parts.len() >= 2 {
                let last_is_path = parts.last().is_some_and(|p| p.parse::<u64>().is_err());
                if last_is_path {
                    parts[..parts.len() - 1].join(" ")
                } else {
                    parts.join(" ")
                }
            } else {
                line.trim().to_string()
            }
        }
    }
}

/// Format multiple files as a compact table
fn format_multi_line(lines: &[&str], mode: &WcMode) -> String {
    let mut result = Vec::new();

    // Find common directory prefix to shorten paths
    let paths: Vec<&str> = lines
        .iter()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.last().copied()
        })
        .filter(|p| *p != "total")
        .collect();

    let common_prefix = find_common_prefix(&paths);

    for line in lines {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let is_total = parts.last().is_some_and(|p| *p == "total");

        match mode {
            WcMode::Lines | WcMode::Words | WcMode::Bytes | WcMode::Chars => {
                if is_total {
                    result.push(format!("Σ {}", parts.first().unwrap_or(&"0")));
                } else {
                    let name = strip_prefix(parts.last().unwrap_or(&""), &common_prefix);
                    result.push(format!("{} {}", parts.first().unwrap_or(&"0"), name));
                }
            }
            WcMode::Full => {
                if is_total {
                    result.push(format!(
                        "Σ {}L {}W {}B",
                        parts.first().unwrap_or(&"0"),
                        parts.get(1).unwrap_or(&"0"),
                        parts.get(2).unwrap_or(&"0"),
                    ));
                } else if parts.len() >= 4 {
                    let name = strip_prefix(parts[3], &common_prefix);
                    result.push(format!(
                        "{}L {}W {}B {}",
                        parts[0], parts[1], parts[2], name
                    ));
                } else {
                    result.push(line.trim().to_string());
                }
            }
            WcMode::Mixed => {
                if is_total {
                    let nums: Vec<&str> = parts[..parts.len() - 1].to_vec();
                    result.push(format!("Σ {}", nums.join(" ")));
                } else if parts.len() >= 2 {
                    let last_is_path = parts.last().is_some_and(|p| p.parse::<u64>().is_err());
                    if last_is_path {
                        let name = strip_prefix(parts.last().unwrap_or(&""), &common_prefix);
                        let nums: Vec<&str> = parts[..parts.len() - 1].to_vec();
                        result.push(format!("{} {}", nums.join(" "), name));
                    } else {
                        result.push(parts.join(" "));
                    }
                } else {
                    result.push(line.trim().to_string());
                }
            }
        }
    }

    result.join("\n")
}

/// Find common directory prefix among paths
fn find_common_prefix(paths: &[&str]) -> String {
    if paths.len() <= 1 {
        return String::new();
    }

    let first = paths[0];
    let prefix = if let Some(pos) = first.rfind('/') {
        &first[..=pos]
    } else {
        return String::new();
    };

    if paths.iter().all(|p| p.starts_with(prefix)) {
        return prefix.to_string();
    }

    // Try shorter prefixes by removing right-most segments
    let mut candidate = prefix.to_string();
    while !candidate.is_empty() {
        if paths.iter().all(|p| p.starts_with(&candidate)) {
            return candidate;
        }
        if let Some(pos) = candidate[..candidate.len() - 1].rfind('/') {
            candidate.truncate(pos + 1);
        } else {
            return String::new();
        }
    }
    String::new()
}

/// Strip common prefix from a path
fn strip_prefix<'a>(path: &'a str, prefix: &str) -> &'a str {
    if prefix.is_empty() {
        return path;
    }
    path.strip_prefix(prefix).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_file_full() {
        let raw = "      30      96     978 scripts/find_duplicate_attrs.py\n";
        let result = filter_wc_output(raw, &WcMode::Full);
        assert_eq!(result, "30L 96W 978B");
    }

    #[test]
    fn test_single_file_lines_only() {
        let raw = "      30 scripts/find_duplicate_attrs.py\n";
        let result = filter_wc_output(raw, &WcMode::Lines);
        assert_eq!(result, "30");
    }

    #[test]
    fn test_single_file_words_only() {
        let raw = "      96 scripts/find_duplicate_attrs.py\n";
        let result = filter_wc_output(raw, &WcMode::Words);
        assert_eq!(result, "96");
    }

    #[test]
    fn test_stdin_full() {
        let raw = "      30      96     978\n";
        let result = filter_wc_output(raw, &WcMode::Full);
        assert_eq!(result, "30L 96W 978B");
    }

    #[test]
    fn test_stdin_lines() {
        let raw = "      30\n";
        let result = filter_wc_output(raw, &WcMode::Lines);
        assert_eq!(result, "30");
    }

    #[test]
    fn test_multi_file_lines() {
        let raw = "      30 src/main.rs\n      50 src/lib.rs\n      80 total\n";
        let result = filter_wc_output(raw, &WcMode::Lines);
        assert_eq!(result, "30 main.rs\n50 lib.rs\nΣ 80");
    }

    #[test]
    fn test_multi_file_full() {
        let raw = "      30      96     978 src/main.rs\n      50     120    1500 src/lib.rs\n      80     216    2478 total\n";
        let result = filter_wc_output(raw, &WcMode::Full);
        assert_eq!(
            result,
            "30L 96W 978B main.rs\n50L 120W 1500B lib.rs\nΣ 80L 216W 2478B"
        );
    }

    #[test]
    fn test_detect_mode_full() {
        let args: Vec<String> = vec!["file.py".into()];
        assert_eq!(detect_mode(&args), WcMode::Full);
    }

    #[test]
    fn test_detect_mode_lines() {
        let args: Vec<String> = vec!["-l".into(), "file.py".into()];
        assert_eq!(detect_mode(&args), WcMode::Lines);
    }

    #[test]
    fn test_detect_mode_mixed() {
        let args: Vec<String> = vec!["-lw".into(), "file.py".into()];
        assert_eq!(detect_mode(&args), WcMode::Mixed);
    }

    #[test]
    fn test_detect_mode_separate_flags() {
        let args: Vec<String> = vec!["-l".into(), "-w".into(), "file.py".into()];
        assert_eq!(detect_mode(&args), WcMode::Mixed);
    }

    #[test]
    fn test_common_prefix() {
        let paths = vec!["src/main.rs", "src/lib.rs", "src/utils.rs"];
        assert_eq!(find_common_prefix(&paths), "src/");
    }

    #[test]
    fn test_no_common_prefix() {
        let paths = vec!["main.rs", "lib.rs"];
        assert_eq!(find_common_prefix(&paths), "");
    }

    #[test]
    fn test_deep_common_prefix() {
        let paths = vec!["src/cmd/wc.rs", "src/cmd/ls.rs"];
        assert_eq!(find_common_prefix(&paths), "src/cmd/");
    }

    #[test]
    fn test_empty() {
        let raw = "";
        let result = filter_wc_output(raw, &WcMode::Full);
        assert_eq!(result, "");
    }

    // ---- native (Windows) counting: pure, runs on every platform ----

    #[test]
    fn test_count_bytes() {
        let c = count_bytes(b"hello world\nfoo\n");
        assert_eq!(c.lines, 2);
        assert_eq!(c.words, 3);
        assert_eq!(c.bytes, 16);
        assert_eq!(c.chars, 16);
        // unterminated final line still counts its words but not a new line
        let c2 = count_bytes(b"a b");
        assert_eq!((c2.lines, c2.words, c2.bytes), (0, 2, 3));
        // multibyte: bytes != chars
        let c3 = count_bytes("é\n".as_bytes());
        assert_eq!((c3.lines, c3.bytes, c3.chars), (1, 3, 2));
    }

    #[test]
    fn test_selected_columns() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // default (l,w,-,b)
        assert_eq!(selected_columns(&a(&["f"])), (true, true, false, true));
        // -l only
        assert_eq!(
            selected_columns(&a(&["-l", "f"])),
            (true, false, false, false)
        );
        // -lw combined → lines + words (Mixed order)
        assert_eq!(
            selected_columns(&a(&["-lw", "f"])),
            (true, true, false, false)
        );
    }

    #[test]
    fn test_native_roundtrip_single_and_multi() {
        let c = count_bytes(b"hello world\nfoo\n"); // 2 3 16 16
        // Single file, default columns -> compact "2L 3W 16B" (path dropped).
        let raw = emit_wc_line(Some("f.txt"), &c, (true, true, false, true)) + "\n";
        assert_eq!(filter_wc_output(&raw, &WcMode::Full), "2L 3W 16B");

        // Two files + total, lines only.
        let raw = format!(
            "{}\n{}\n{}",
            emit_wc_line(Some("a"), &c, (true, false, false, false)),
            emit_wc_line(Some("b"), &c, (true, false, false, false)),
            emit_wc_line(Some("total"), &c, (true, false, false, false)),
        );
        assert_eq!(filter_wc_output(&raw, &WcMode::Lines), "2 a\n2 b\nΣ 2");
    }
}
