//! tree command - proxy to native tree with token-optimized output
//!
//! This module proxies to the native `tree` command and filters the output
//! to reduce token usage while preserving structure visibility.
//!
//! Token optimization: automatically excludes noise directories via -I pattern
//! unless -a flag is present (respecting user intent).

use super::constants::NOISE_DIRS;
use crate::core::runner::{self, RunOptions};
use crate::core::utils::{ChildArgExt, resolved_command, tool_exists};
use anyhow::Result;
use std::path::Path;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    // Windows ships a *different* `tree` (tree.com) that rejects GNU's `-I`
    // and other flags (it fails with "参数太多"/"Too many parameters"), and
    // `tool_exists("tree")` happily matches it. Draw the tree natively with
    // std::fs instead, reproducing rtk's compact output.
    if cfg!(windows) {
        return run_native(args, verbose);
    }

    if !tool_exists("tree") {
        anyhow::bail!(
            "tree command not found. Install it first:\n\
             - macOS: brew install tree\n\
             - Ubuntu/Debian: sudo apt install tree\n\
             - Fedora/RHEL: sudo dnf install tree\n\
             - Arch: sudo pacman -S tree"
        );
    }

    let mut cmd = resolved_command("tree");

    let show_all = args.iter().any(|a| a == "-a" || a == "--all");
    let has_ignore = args.iter().any(|a| a == "-I" || a.starts_with("--ignore="));

    if !show_all && !has_ignore {
        let ignore_pattern = NOISE_DIRS.join("|");
        cmd.arg("-I").arg(&ignore_pattern);
    }

    cmd.child_args(args);

    runner::run_filtered(
        cmd,
        "tree",
        &args.join(" "),
        |raw| {
            let filtered = filter_tree_output(raw);
            if verbose > 0 {
                eprintln!(
                    "Lines: {} → {} ({}% reduction)",
                    raw.lines().count(),
                    filtered.lines().count(),
                    if raw.lines().count() > 0 {
                        100 - (filtered.lines().count() * 100 / raw.lines().count())
                    } else {
                        0
                    }
                );
            }
            filtered
        },
        RunOptions::stdout_only()
            .early_exit_on_failure()
            .no_trailing_newline(),
    )
}

fn filter_tree_output(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();

    if lines.is_empty() {
        return "\n".to_string();
    }

    let mut filtered_lines = Vec::new();

    for line in lines {
        // Skip the final summary line (e.g., "5 directories, 23 files")
        if line.contains("director") && line.contains("file") {
            continue;
        }

        // Skip empty lines at the end
        if line.trim().is_empty() && filtered_lines.is_empty() {
            continue;
        }

        filtered_lines.push(line);
    }

    // Remove trailing empty lines
    while filtered_lines.last().is_some_and(|l| l.trim().is_empty()) {
        filtered_lines.pop();
    }

    filtered_lines.join("\n") + "\n"
}

/// A node in the natively-built tree. Kept as data so [`render_nodes`] can be
/// unit-tested without touching the filesystem.
enum Node {
    Dir(String, Vec<Node>),
    File(String),
}

impl Node {
    fn name(&self) -> &str {
        match self {
            Node::Dir(n, _) | Node::File(n) => n,
        }
    }
}

/// `rtk tree` on Windows: read the directory tree with std::fs (no external
/// `tree`) and draw it with the same box glyphs the Unix output uses.
fn run_native(args: &[String], verbose: u8) -> Result<i32> {
    let show_all = args.iter().any(|a| a == "-a" || a == "--all");
    let max_level = parse_level(args);

    // First non-flag token that isn't the value consumed by `-L`/`--level`.
    let root = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && !is_level_value(args, *i))
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| ".".to_string());

    let path = Path::new(&root);
    if !path.exists() {
        eprintln!("rtk tree: cannot access '{root}': No such file or directory");
        return Ok(1);
    }

    let children = if path.is_dir() {
        children_of(path, show_all, max_level, 1)
    } else {
        Vec::new()
    };

    let mut lines = vec![root.clone()];
    render_nodes(&children, "", &mut lines);

    if verbose > 0 {
        eprintln!("rtk tree: {} lines (native)", lines.len());
    }
    // Match the Unix path's `no_trailing_newline()` rendering.
    print!("{}", lines.join("\n"));
    Ok(0)
}

/// `-L n` / `--level n`: maximum display depth (root's children = level 1).
fn parse_level(args: &[String]) -> Option<usize> {
    args.iter()
        .position(|a| a == "-L" || a == "--level")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
}

/// True if the token at `idx` is the value that follows `-L`/`--level`.
fn is_level_value(args: &[String], idx: usize) -> bool {
    idx > 0 && matches!(args[idx - 1].as_str(), "-L" | "--level")
}

/// Directories before files (the common `tree --dirsfirst` reading), then by
/// name; noise dirs and dotfiles are hidden unless `-a`. `cur` is the 1-based
/// depth of these entries relative to the root.
fn children_of(dir: &Path, show_all: bool, max_level: Option<usize>, cur: usize) -> Vec<Node> {
    if max_level.is_some_and(|m| cur > m) {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_all && (name.starts_with('.') || NOISE_DIRS.contains(&name.as_str())) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        nodes.push(if is_dir {
            Node::Dir(
                name,
                children_of(&entry.path(), show_all, max_level, cur + 1),
            )
        } else {
            Node::File(name)
        });
    }
    nodes.sort_by(|a, b| match (a, b) {
        (Node::Dir(..), Node::File(..)) => std::cmp::Ordering::Less,
        (Node::File(..), Node::Dir(..)) => std::cmp::Ordering::Greater,
        _ => a.name().cmp(b.name()),
    });
    nodes
}

/// Draw `children` under `prefix`, appending box-drawing lines to `out`.
fn render_nodes(children: &[Node], prefix: &str, out: &mut Vec<String>) {
    for (i, child) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        let (connector, cont) = if last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        match child {
            Node::File(name) => out.push(format!("{prefix}{connector}{name}")),
            Node::Dir(name, sub) => {
                out.push(format!("{prefix}{connector}{name}"));
                render_nodes(sub, &format!("{prefix}{cont}"), out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_removes_summary() {
        let input = ".\n├── src\n│   └── main.rs\n└── Cargo.toml\n\n2 directories, 3 files\n";
        let output = filter_tree_output(input);
        assert!(!output.contains("directories"));
        assert!(!output.contains("files"));
        assert!(output.contains("main.rs"));
        assert!(output.contains("Cargo.toml"));
    }

    #[test]
    fn test_filter_preserves_structure() {
        let input = ".\n├── src\n│   ├── main.rs\n│   └── lib.rs\n└── tests\n    └── test.rs\n";
        let output = filter_tree_output(input);
        assert!(output.contains("├──"));
        assert!(output.contains("│"));
        assert!(output.contains("└──"));
        assert!(output.contains("main.rs"));
        assert!(output.contains("test.rs"));
    }

    #[test]
    fn test_filter_handles_empty() {
        let input = "";
        let output = filter_tree_output(input);
        assert_eq!(output, "\n");
    }

    #[test]
    fn test_filter_removes_trailing_empty_lines() {
        let input = ".\n├── file.txt\n\n\n";
        let output = filter_tree_output(input);
        assert_eq!(output.matches('\n').count(), 2); // Root + file.txt + final newline
    }

    #[test]
    fn test_filter_summary_variations() {
        // Test different summary formats
        let inputs = vec![
            (".\n└── file.txt\n\n0 directories, 1 file\n", "1 file"),
            (".\n└── file.txt\n\n1 directory, 0 files\n", "1 directory"),
            (".\n└── file.txt\n\n10 directories, 25 files\n", "25 files"),
        ];

        for (input, summary_fragment) in inputs {
            let output = filter_tree_output(input);
            assert!(
                !output.contains(summary_fragment),
                "Should remove summary '{}' from output",
                summary_fragment
            );
            assert!(
                output.contains("file.txt"),
                "Should preserve file.txt in output"
            );
        }
    }

    #[test]
    fn test_noise_dirs_constant() {
        // Verify NOISE_DIRS contains expected patterns
        assert!(NOISE_DIRS.contains(&"node_modules"));
        assert!(NOISE_DIRS.contains(&".git"));
        assert!(NOISE_DIRS.contains(&"target"));
        assert!(NOISE_DIRS.contains(&"__pycache__"));
        assert!(NOISE_DIRS.contains(&".next"));
        assert!(NOISE_DIRS.contains(&"dist"));
        assert!(NOISE_DIRS.contains(&"build"));
    }

    // ---- native (Windows) renderer: pure, runs on every platform ----

    fn dir(name: &str, kids: Vec<Node>) -> Node {
        Node::Dir(name.into(), kids)
    }
    fn file(name: &str) -> Node {
        Node::File(name.into())
    }

    #[test]
    fn test_render_nodes_uses_box_glyphs() {
        let tree = vec![
            dir("src", vec![file("main.rs"), file("lib.rs")]),
            file("Cargo.toml"),
        ];
        // run_native seeds the first line with the root path, then renders.
        let mut lines = vec![".".to_string()];
        render_nodes(&tree, "", &mut lines);
        assert_eq!(
            lines,
            vec![
                ".",
                "├── src",
                "│   ├── main.rs",
                "│   └── lib.rs",
                "└── Cargo.toml",
            ]
        );
    }

    #[test]
    fn test_render_nested_prefixes() {
        let tree = vec![dir("a", vec![dir("b", vec![file("c.txt")]), file("z.txt")])];
        let mut lines = vec![".".to_string()];
        render_nodes(&tree, "", &mut lines);
        assert_eq!(
            lines,
            vec![
                ".",
                "└── a",
                "    ├── b",
                "    │   └── c.txt",
                "    └── z.txt",
            ]
        );
    }

    #[test]
    fn test_parse_level() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_level(&a(&["-L", "2"])), Some(2));
        assert_eq!(parse_level(&a(&["--level", "3"])), Some(3));
        assert_eq!(parse_level(&a(&["."])), None);
    }

    #[test]
    fn test_is_level_value_skips_level_arg() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let args = a(&["-L", "2", "src"]);
        assert!(is_level_value(&args, 1)); // "2"
        assert!(!is_level_value(&args, 2)); // "src" is a real path
    }
}
