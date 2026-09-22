//! `rtk du` — disk usage with compact output.
//!
//! Unix: thin proxy over the system `du`, preserving its exact semantics.
//! Windows: GNU `du` does not exist, and the rtk hook rewrites `du …` into
//! `rtk du …` (see `discover::rules`), so the external fallback used to die with
//! "Binary 'du' not found on PATH" (exit 127). This module walks the tree with
//! `std::fs` and reproduces the common du surface: `-s/--summarize`,
//! `-a/--all`, `-c/--total`, `-h/--human-readable`, `-b/-k/-m` units,
//! `-d N` / `--max-depth[=N]`, and multiple paths. Sizes are apparent file
//! lengths (Windows block/compression accounting is not approximated).

use anyhow::Result;

/// One `du` run's parsed options.
#[cfg(windows)]
#[derive(Debug, PartialEq)]
struct DuOpts {
    all_files: bool,
    total: bool,
    human: bool,
    /// Bytes per reported unit when not human-readable (1 = -b, 1024 = default,
    /// 1 MiB = -m).
    divisor: u64,
    max_depth: usize,
    paths: Vec<String>,
}

#[cfg(windows)]
impl Default for DuOpts {
    fn default() -> Self {
        Self {
            all_files: bool::default(),
            total: bool::default(),
            human: bool::default(),
            divisor: 1024,
            max_depth: usize::MAX,
            paths: Vec::new(),
        }
    }
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    #[cfg(unix)]
    {
        // The system du knows its own semantics best: run and pass through.
        use crate::core::runner::{self, RunOptions};
        let mut cmd = crate::core::utils::resolved_command("du");
        cmd.args(args);
        return runner::run_filtered(
            cmd,
            "du",
            &args.join(" "),
            |raw| raw.to_string(),
            RunOptions::stdout_only(),
        );
    }
    #[cfg(not(unix))]
    {
        let _ = verbose;
        run_native(args)
    }
}

#[cfg(windows)]
fn run_native(args: &[String]) -> Result<i32> {
    let opts = parse_args(args)?;
    let mut rows: Vec<(u64, std::path::PathBuf)> = Vec::new();
    let mut grand: u64 = 0;
    let mut had_err = false;
    for p in &opts.paths {
        let path = std::path::Path::new(p);
        if !path.exists() {
            eprintln!("du: cannot access '{p}': No such file or directory");
            had_err = true;
            continue;
        }
        grand += walk(path, &opts, &mut rows, 0);
    }
    let mut out = format_rows(&rows, &opts);
    if opts.total {
        out.push_str(&format!("{}\tTOTAL\n", render_size(grand, &opts)));
    }
    print!("{out}");
    Ok(if had_err { 1 } else { 0 })
}

/// Parse the supported du surface, including bundled short flags (`-sh`,
/// `-ad 2`-style clusters) and `--max-depth=N`.
#[cfg(windows)]
fn parse_args(args: &[String]) -> Result<DuOpts> {
    let mut o = DuOpts::default();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--" {
            for p in it.by_ref() {
                o.paths.push(p.clone());
            }
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "summarize" => o.max_depth = 0,
                "all" => o.all_files = true,
                "total" => o.total = true,
                "human-readable" => o.human = true,
                "bytes" => o.divisor = 1,
                _ if long.starts_with("max-depth=") => {
                    o.max_depth = parse_depth(long.trim_start_matches("max-depth="))?;
                }
                "max-depth" => {
                    let v = it
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("du: option requires an argument -- 'd'"))?;
                    o.max_depth = parse_depth(v)?;
                }
                other => anyhow::bail!("rtk du (native Windows): unsupported option --{other}"),
            }
            continue;
        }
        if let Some(cluster) = a.strip_prefix('-') {
            let mut chars = cluster.chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    's' => o.max_depth = 0,
                    'a' => o.all_files = true,
                    'c' => o.total = true,
                    'h' => o.human = true,
                    'b' => o.divisor = 1,
                    'k' => o.divisor = 1024,
                    'm' => o.divisor = 1024 * 1024,
                    'd' => {
                        // `-d2` inline value, else the next argument.
                        let rest: String = chars.by_ref().collect();
                        let value = if rest.is_empty() {
                            it.next().cloned().ok_or_else(|| {
                                anyhow::anyhow!("du: option requires an argument -- 'd'")
                            })?
                        } else {
                            rest
                        };
                        o.max_depth = parse_depth(&value)?;
                    }
                    other => anyhow::bail!("rtk du (native Windows): unsupported option -{other}"),
                }
            }
            continue;
        }
        o.paths.push(a.clone());
    }
    if o.paths.is_empty() {
        o.paths.push(".".to_string());
    }
    Ok(o)
}

#[cfg(windows)]
fn parse_depth(v: &str) -> Result<usize> {
    v.parse::<usize>()
        .map_err(|_| anyhow::anyhow!("du: invalid depth limit: '{v}'"))
}

/// Recursively sum `dir`; emit rows bottom-up (like du) for entries within
/// `max_depth`. Symlinks are not followed, matching default du behaviour.
#[cfg(windows)]
fn walk(
    dir: &std::path::Path,
    opts: &DuOpts,
    rows: &mut Vec<(u64, std::path::PathBuf)>,
    depth: usize,
) -> u64 {
    let mut sum: u64 = 0;
    let mut subdirs: Vec<std::path::PathBuf> = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    subdirs.push(entry.path());
                } else if let Ok(m) = entry.metadata() {
                    sum += m.len();
                    if opts.all_files && depth < opts.max_depth {
                        rows.push((m.len(), entry.path()));
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("du: cannot read directory '{}': {e}", dir.display());
        }
    }
    for sd in subdirs {
        sum += walk(&sd, opts, rows, depth + 1);
    }
    if depth <= opts.max_depth {
        rows.push((sum, dir.to_path_buf()));
    }
    sum
}

#[cfg(windows)]
fn format_rows(rows: &[(u64, std::path::PathBuf)], opts: &DuOpts) -> String {
    let mut s = String::new();
    for (size, path) in rows {
        s.push_str(&render_size(*size, opts));
        s.push('\t');
        s.push_str(&path.display().to_string());
        s.push('\n');
    }
    s
}

/// `du -h` renders one decimal below 10, plain integers from 10 up; the
/// non-human modes report `ceil(bytes / divisor)` like du's block counts.
#[cfg(windows)]
fn render_size(bytes: u64, opts: &DuOpts) -> String {
    if !opts.human {
        return bytes.div_ceil(opts.divisor).to_string();
    }
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut i = 1usize;
    while i < UNITS.len() - 1 && v >= 1024.0 {
        v /= 1024.0;
        i += 1;
    }
    if v < 10.0 {
        format!("{:.1}{}", v, UNITS[i])
    } else {
        format!("{:.0}{}", v, UNITS[i])
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_defaults_to_cwd() {
        let o = parse_args(&a(&[])).unwrap();
        assert_eq!(o.paths, vec!["."]);
        assert_eq!(o.max_depth, usize::MAX);
        assert_eq!(o.divisor, 1024);
    }

    #[test]
    fn parse_bundled_flags() {
        let o = parse_args(&a(&["-sh", "."])).unwrap();
        assert!(o.human);
        assert_eq!(o.max_depth, 0, "-s clamps depth like du");
        assert_eq!(o.paths, vec!["."]);
    }

    #[test]
    fn parse_depth_forms() {
        assert_eq!(parse_args(&a(&["-d", "2"])).unwrap().max_depth, 2);
        assert_eq!(parse_args(&a(&["-d2"])).unwrap().max_depth, 2);
        assert_eq!(parse_args(&a(&["--max-depth", "3"])).unwrap().max_depth, 3);
        assert_eq!(parse_args(&a(&["--max-depth=4"])).unwrap().max_depth, 4);
        assert!(parse_args(&a(&["-d", "x"])).is_err());
    }

    #[test]
    fn parse_rejects_unknown_flags() {
        assert!(parse_args(&a(&["--si"])).is_err());
        assert!(parse_args(&a(&["-B", "512"])).is_err());
    }

    #[test]
    fn render_units_and_human() {
        let blocks = DuOpts {
            human: false,
            divisor: 1024,
            ..Default::default()
        };
        assert_eq!(render_size(0, &blocks), "0");
        assert_eq!(render_size(1, &blocks), "1", "du rounds up");
        assert_eq!(render_size(2048, &blocks), "2");

        let bytes = DuOpts {
            human: false,
            divisor: 1,
            ..Default::default()
        };
        assert_eq!(render_size(12345, &bytes), "12345");

        let human = DuOpts {
            human: true,
            ..Default::default()
        };
        assert_eq!(render_size(999, &human), "999B");
        assert_eq!(render_size(1536, &human), "1.5K");
        assert_eq!(render_size(20 * 1024 * 1024, &human), "20M");
        assert_eq!(render_size(3 * 1024 * 1024 * 1024, &human), "3.0G");
    }

    #[test]
    fn walks_a_temp_tree_bottom_up() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x64").unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.txt"), "y100").unwrap();

        let opts = parse_args(&a(&[tmp.path().to_str().unwrap()])).unwrap();
        let mut rows = Vec::new();
        let total = walk(tmp.path(), &opts, &mut rows, 0);
        assert_eq!(total, 7); // "x64" (3 bytes) + "y100" (4 bytes)
        assert_eq!(rows.len(), 2, "sub emitted before parent");
        assert!(rows[0].1.ends_with("sub"));
        assert!(rows[1].1 == tmp.path());
    }

    #[test]
    fn summarize_emits_single_row() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "abc").unwrap();
        let opts = parse_args(&a(&["-sh", tmp.path().to_str().unwrap()])).unwrap();
        let mut rows = Vec::new();
        walk(tmp.path(), &opts, &mut rows, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 3);
    }
}
