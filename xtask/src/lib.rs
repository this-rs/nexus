//! Repository chores for nexus, as a workspace crate (`cargo xtask …`).
//!
//! Two commands, both about `docs/diagrams/`, both offline:
//!
//! * `diagrams index [--write | --check]` derives `INDEX.yml` from the headers
//!   of the `.mmd` files ([`index`]);
//! * `diagrams drift [--base <ref>]` fails a pull request that changes code a
//!   diagram owns without saying so ([`drift`]).
//!
//! Everything that decides is a pure function over strings; [`run`] is the only
//! place that reads the disk or calls `git`.

pub mod drift;
pub mod glob;
pub mod header;
pub mod index;

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// Where the diagrams live, relative to the repository root.
pub const DIAGRAM_DIR: &str = "docs/diagrams";
/// The generated index, relative to the repository root.
pub const INDEX: &str = "docs/diagrams/INDEX.yml";

const USAGE: &str = "usage:
  cargo xtask diagrams index            check the headers and print the ownership map
  cargo xtask diagrams index --write    regenerate docs/diagrams/INDEX.yml
  cargo xtask diagrams index --check    also fail when the committed INDEX.yml is stale
  cargo xtask diagrams drift [--base <ref>]   (default origin/main)
";

/// Runs one command from inside the repository containing `cwd`.
///
/// Everything printed goes to `out`. Returns the process exit code: 0 on
/// success, 1 when a check fails, 2 when the command line or `git` is wrong.
pub fn run(args: &[String], cwd: &Path, out: &mut String) -> u8 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["diagrams", "index"] => run_index(cwd, IndexMode::Print, out),
        ["diagrams", "index", "--write"] => run_index(cwd, IndexMode::Write, out),
        ["diagrams", "index", "--check"] => run_index(cwd, IndexMode::Check, out),
        ["diagrams", "drift"] => run_drift(cwd, "origin/main", out),
        ["diagrams", "drift", "--base", base] => run_drift(cwd, base, out),
        _ => {
            out.push_str(USAGE);
            return 2;
        },
    };
    match result {
        Ok(code) => code,
        Err(why) => {
            let _ = writeln!(out, "error: {why}");
            2
        },
    }
}

#[derive(Clone, Copy, PartialEq)]
enum IndexMode {
    Print,
    Write,
    Check,
}

/// `git <args>` in `cwd`; its stdout, or why it failed.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn nul_separated(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The repository root, and every file in it that is tracked or would be
/// (untracked and not ignored): a file about to be committed counts already.
fn repository(cwd: &Path) -> Result<(std::path::PathBuf, Vec<String>), String> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = std::path::PathBuf::from(root.trim());
    let listed = git(
        &root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut files: Vec<String> = nul_separated(&listed)
        .into_iter()
        // A file deleted from the working tree but not yet from the index.
        .filter(|f| root.join(f).is_file())
        .collect();
    files.sort();
    files.dedup();
    Ok((root, files))
}

/// Every `.mmd` of [`DIAGRAM_DIR`] (the template aside) with its text.
fn read_diagrams(root: &Path, files: &[String]) -> Result<Vec<(String, String)>, String> {
    let prefix = format!("{DIAGRAM_DIR}/");
    let mut diagrams = Vec::new();
    for file in files {
        let Some(name) = file.strip_prefix(&prefix) else {
            continue;
        };
        if name.contains('/') || !name.ends_with(".mmd") || name.contains("_TEMPLATE") {
            continue;
        }
        let text = std::fs::read_to_string(root.join(file))
            .map_err(|e| format!("cannot read {file}: {e}"))?;
        diagrams.push((file.clone(), text));
    }
    Ok(diagrams)
}

fn run_index(cwd: &Path, mode: IndexMode, out: &mut String) -> Result<u8, String> {
    let (root, files) = repository(cwd)?;
    let diagrams = read_diagrams(&root, &files)?;
    if diagrams.is_empty() {
        let _ = writeln!(
            out,
            "no diagram found in {DIAGRAM_DIR}/ — nothing to derive"
        );
        return Ok(1);
    }
    let index = index::derive(&diagrams, &files);
    out.push_str(&index.report());
    if !index.problems.is_empty() {
        let _ = writeln!(
            out,
            "\n{} problem(s). INDEX.yml not written.",
            index.problems.len()
        );
        return Ok(1);
    }
    let rendered = index.render();
    match mode {
        IndexMode::Print => {},
        IndexMode::Write => {
            std::fs::write(root.join(INDEX), rendered)
                .map_err(|e| format!("cannot write {INDEX}: {e}"))?;
            let _ = writeln!(out, "\n{INDEX} written");
        },
        IndexMode::Check => {
            // Line endings aside: a Windows checkout may hold CRLF.
            let committed = std::fs::read_to_string(root.join(INDEX)).unwrap_or_default();
            if committed.replace("\r\n", "\n") != rendered {
                let _ = writeln!(
                    out,
                    "\n{INDEX} is stale — run `cargo xtask diagrams index --write`"
                );
                return Ok(1);
            }
            let _ = writeln!(out, "\n{INDEX} is current");
        },
    }
    Ok(0)
}

fn run_drift(cwd: &Path, base: &str, out: &mut String) -> Result<u8, String> {
    let (root, files) = repository(cwd)?;
    let changed = nul_separated(&git(
        &root,
        &["diff", "--name-only", "-z", &format!("{base}...HEAD")],
    )?);
    let messages = nul_separated(&git(
        &root,
        &["log", "--format=%B%x00", &format!("{base}..HEAD")],
    )?);

    let mut problems = Vec::new();
    let mut owners = Vec::new();
    for (path, text) in read_diagrams(&root, &files)? {
        let (parsed, errors) = header::parse(&path, &text);
        problems.extend(errors);
        if let Some(h) = parsed {
            owners.push(drift::Owner {
                name: h.name,
                file: path,
                covers: h.covers,
            });
        }
    }
    let (trailers, trailer_problems) = drift::parse_trailers(&messages);
    let found = drift::find(&changed, &owners, &trailers);
    problems.extend(trailer_problems);
    problems.extend(found.problems);

    let _ = writeln!(
        out,
        "{} changed file(s) against {base} | {} diagrams",
        changed.len(),
        owners.len()
    );
    for (name, reason) in &trailers {
        let _ = writeln!(out, "  stated unchanged: {name} — {reason}");
    }
    if !found.unowned.is_empty() {
        out.push_str("  changed, owned by no diagram (not an error):\n");
        for file in &found.unowned {
            let _ = writeln!(out, "    {file}");
        }
    }
    for problem in &problems {
        let _ = writeln!(out, "  ERROR {problem}");
    }
    if problems.is_empty() {
        out.push_str("no drift\n");
        Ok(0)
    } else {
        let _ = writeln!(out, "\n{} problem(s).", problems.len());
        Ok(1)
    }
}
