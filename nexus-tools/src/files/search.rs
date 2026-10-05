//! `Glob` and `Grep`, on the crates ripgrep is made of (`ignore`, `globset`, `regex`): no
//! external `rg` binary (decision N17).
//!
//! The semantics are ripgrep's: the same `.gitignore` handling, the same `--type` names and
//! `--glob` overrides, the same line-by-line matching and multiline mode. They are checked
//! against a real `rg` on a test tree (`tests/search.rs`, recorded goldens when `rg` is not
//! installed).
//!
//! Claude Code 2.1.287's `claude -p` has no `Glob`/`Grep` to record, so the output
//! *labels* (`Found N files`, the count footer) follow Claude Code's documented behaviour
//! and are not backed by a recording.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};

use super::FileConfig;
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// Entries returned when `head_limit` is not given.
pub const DEFAULT_HEAD_LIMIT: usize = 250;
/// Paths returned by `Glob`.
pub const GLOB_LIMIT: usize = 100;
/// Files bigger than this are not searched (they are data, not source).
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Directories that are never searched.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj"];

/// Stops a blocking search when the call that started it is dropped (the request was
/// cancelled): a `spawn_blocking` task cannot be aborted, so it has to be asked.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false).filter_entry(|entry| {
        !(entry.file_type().is_some_and(|t| t.is_dir())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| VCS_DIRS.contains(&name)))
    });
    builder
}

fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

/// The `Glob` tool.
#[derive(Debug)]
pub struct GlobTool {
    config: FileConfig,
}

impl GlobTool {
    /// A `Glob` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Finds files by glob pattern (`**/*.rs`, `src/**/*.{ts,tsx}`). Returns paths sorted by \
         modification time, newest first, honouring .gitignore. At most 100 paths."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search (default: the working directory)"}
            },
            "required": ["pattern"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations::read_only()
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        let Some(pattern) = arguments.get("pattern").and_then(Value::as_str) else {
            return ToolResult::error("`pattern` is required and must be a string");
        };
        let root = match search_root(&self.config, arguments.get("path")) {
            Ok(root) => root,
            Err(message) => return ToolResult::error(message),
        };
        let matcher = match globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
        {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => return ToolResult::error(format!("Invalid glob pattern: {error}")),
        };
        let cwd = self.config.scope.cwd().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopOnDrop(Arc::clone(&stop));
        let found = tokio::task::spawn_blocking(move || {
            let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
            for entry in walker(&root).build().flatten() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                if matcher.is_match(relative) {
                    files.push((modified(entry.path()), entry.path().to_path_buf()));
                }
            }
            files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            files
        })
        .await
        .unwrap_or_default();
        if found.is_empty() {
            return ToolResult::ok("No files found");
        }
        let truncated = found.len() > GLOB_LIMIT;
        let mut lines: Vec<String> = found
            .iter()
            .take(GLOB_LIMIT)
            .map(|(_, path)| shown(path, &cwd))
            .collect();
        if truncated {
            lines.push(
                "(Results are truncated. Consider using a more specific path or pattern.)".into(),
            );
        }
        ToolResult::ok(lines.join("\n"))
    }
}

fn search_root(config: &FileConfig, path: Option<&Value>) -> Result<PathBuf, String> {
    match path.and_then(Value::as_str) {
        None | Some("") => Ok(config.scope.cwd().to_path_buf()),
        Some(given) => {
            let resolved = config.scope.resolve(given).map_err(|e| e.to_string())?;
            if resolved.exists() {
                Ok(resolved)
            } else {
                Err(format!(
                    "Path does not exist: {given}. Note: your current working directory is {}.",
                    config.scope.cwd().display()
                ))
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

/// The `Grep` tool.
#[derive(Debug)]
pub struct GrepTool {
    config: FileConfig,
}

impl GrepTool {
    /// A `Grep` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

struct Query {
    regex: regex::Regex,
    multiline: bool,
    mode: OutputMode,
    numbers: bool,
    before: usize,
    after: usize,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Searches file contents with a regular expression (ripgrep semantics, .gitignore \
         honoured). output_mode: files_with_matches (default), content (matching lines, \
         with -A/-B/-C context and -n line numbers) or count. Filter with `glob` or `type`. \
         `head_limit` (default 250, 0 = no limit) and `offset` paginate."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "type": {"type": "string", "description": "ripgrep file type: rust, js, py…"},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                "-i": {"type": "boolean"},
                "-n": {"type": "boolean", "description": "Line numbers (content mode, default true)"},
                "-A": {"type": "integer", "minimum": 0},
                "-B": {"type": "integer", "minimum": 0},
                "-C": {"type": "integer", "minimum": 0},
                "context": {"type": "integer", "minimum": 0},
                "multiline": {"type": "boolean"},
                "head_limit": {"type": "integer", "minimum": 0},
                "offset": {"type": "integer", "minimum": 0}
            },
            "required": ["pattern"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations::read_only()
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        let Some(pattern) = arguments.get("pattern").and_then(Value::as_str) else {
            return ToolResult::error("`pattern` is required and must be a string");
        };
        let flag = |name: &str| arguments.get(name).and_then(Value::as_bool);
        let number = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_u64)
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        };
        let mode = match arguments.get("output_mode").and_then(Value::as_str) {
            None | Some("files_with_matches") => OutputMode::FilesWithMatches,
            Some("content") => OutputMode::Content,
            Some("count") => OutputMode::Count,
            Some(other) => {
                return ToolResult::error(format!(
                    "output_mode must be content, files_with_matches or count, not `{other}`"
                ));
            },
        };
        let multiline = flag("multiline").unwrap_or(false);
        let regex = match RegexBuilder::new(pattern)
            .case_insensitive(flag("-i").unwrap_or(false))
            .dot_matches_new_line(multiline)
            .multi_line(multiline)
            .build()
        {
            Ok(regex) => regex,
            Err(error) => return ToolResult::error(format!("Invalid regular expression: {error}")),
        };
        let context = number("-C").or_else(|| number("context"));
        let query = Query {
            regex,
            multiline,
            mode,
            numbers: flag("-n").unwrap_or(true),
            before: number("-B").or(context).unwrap_or(0),
            after: number("-A").or(context).unwrap_or(0),
        };
        let root = match search_root(&self.config, arguments.get("path")) {
            Ok(root) => root,
            Err(message) => return ToolResult::error(message),
        };
        let mut builder = walker(&root);
        if let Some(glob) = arguments.get("glob").and_then(Value::as_str) {
            let mut overrides = OverrideBuilder::new(&root);
            // Several patterns may be given, separated by spaces: `*.ts *.tsx`.
            for part in glob.split_whitespace() {
                if let Err(error) = overrides.add(part) {
                    return ToolResult::error(format!("Invalid glob `{part}`: {error}"));
                }
            }
            match overrides.build() {
                Ok(built) => {
                    builder.overrides(built);
                },
                Err(error) => return ToolResult::error(format!("Invalid glob: {error}")),
            }
        }
        if let Some(kind) = arguments.get("type").and_then(Value::as_str) {
            let mut types = TypesBuilder::new();
            types.add_defaults();
            types.select(kind);
            match types.build() {
                Ok(built) => {
                    builder.types(built);
                },
                Err(_) => {
                    return ToolResult::error(format!(
                        "Unknown file type `{kind}`. Use a ripgrep type name (rust, js, py, …)."
                    ));
                },
            }
        }
        let head_limit = number("head_limit").unwrap_or(DEFAULT_HEAD_LIMIT);
        let offset = number("offset").unwrap_or(0);
        let cwd = self.config.scope.cwd().to_path_buf();
        let single_file = root.is_file();

        let stop = Arc::new(AtomicBool::new(false));
        let _guard = StopOnDrop(Arc::clone(&stop));
        let hits = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<Hit> = Vec::new();
            for entry in builder.build().flatten() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if let Some(hit) = search_file(entry.path(), &query) {
                    hits.push(hit);
                }
            }
            (hits, query)
        })
        .await;
        let Ok((mut hits, query)) = hits else {
            return ToolResult::error("the search failed unexpectedly");
        };
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        render(hits, &query, &cwd, single_file, Page { head_limit, offset })
    }
}

/// What one file contributed.
struct Hit {
    path: PathBuf,
    /// Lines (1-based) that match, ascending.
    matched: BTreeSet<usize>,
    /// The file's lines, kept only for the content mode.
    lines: Vec<String>,
}

fn search_file(path: &Path, query: &Query) -> Option<Hit> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    // As ripgrep: a NUL byte near the start means "binary", and a binary file is not searched.
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split_terminator('\n').collect();
    let mut matched = BTreeSet::new();
    if query.multiline {
        // Offsets of line starts, to turn a match span into the lines it covers.
        let mut starts = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        for line in &lines {
            starts.push(at);
            at += line.len() + 1;
        }
        let line_of = |offset: usize| starts.partition_point(|s| *s <= offset).max(1);
        for found in query.regex.find_iter(&text) {
            let first = line_of(found.start());
            // A match that ends right after a newline does not cover the next line.
            let last = line_of(found.end().saturating_sub(1).max(found.start()));
            matched.extend(first..=last.min(lines.len().max(1)));
        }
    } else {
        for (index, line) in lines.iter().enumerate() {
            if query
                .regex
                .is_match(line.strip_suffix('\r').unwrap_or(line))
            {
                matched.insert(index + 1);
            }
        }
    }
    if matched.is_empty() {
        return None;
    }
    let kept = if query.mode == OutputMode::Content {
        lines.iter().map(|l| (*l).to_owned()).collect()
    } else {
        Vec::new()
    };
    Some(Hit {
        path: path.to_path_buf(),
        matched,
        lines: kept,
    })
}

struct Page {
    head_limit: usize,
    offset: usize,
}

fn render(hits: Vec<Hit>, query: &Query, cwd: &Path, single_file: bool, page: Page) -> ToolResult {
    if hits.is_empty() {
        return ToolResult::ok(match query.mode {
            OutputMode::FilesWithMatches => "No files found",
            _ => "No matches found",
        });
    }
    let name = |hit: &Hit| shown(&hit.path, cwd);
    let (entries, footer): (Vec<String>, Option<String>) = match query.mode {
        OutputMode::FilesWithMatches => {
            let mut by_time: Vec<&Hit> = hits.iter().collect();
            by_time.sort_by(|a, b| {
                modified(&b.path)
                    .cmp(&modified(&a.path))
                    .then_with(|| a.path.cmp(&b.path))
            });
            (by_time.iter().map(|h| name(h)).collect(), None)
        },
        OutputMode::Count => {
            let total: usize = hits.iter().map(|h| h.matched.len()).sum();
            let entries = hits
                .iter()
                .map(|h| {
                    if single_file {
                        h.matched.len().to_string()
                    } else {
                        format!("{}:{}", name(h), h.matched.len())
                    }
                })
                .collect();
            let footer = format!(
                "Found {total} total occurrence{} across {} file{}.",
                plural(total),
                hits.len(),
                plural(hits.len())
            );
            (entries, Some(footer))
        },
        OutputMode::Content => (content_lines(&hits, query, cwd, single_file), None),
    };

    let total = entries.len();
    let limit = if page.head_limit == 0 {
        usize::MAX
    } else {
        page.head_limit
    };
    let shown_entries: Vec<&String> = entries.iter().skip(page.offset).take(limit).collect();
    let mut out = String::new();
    if query.mode == OutputMode::FilesWithMatches {
        out.push_str(&format!("Found {total} file{}\n", plural(total)));
    }
    out.push_str(
        &shown_entries
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let end = page.offset + shown_entries.len();
    if end < total {
        out.push_str(&format!(
            "\n\n[output limited: showing {}–{} of {total}; call again with offset {end} to continue]",
            page.offset + 1,
            end
        ));
    }
    if let Some(footer) = footer {
        out.push_str(&format!("\n\n{footer}"));
    }
    ToolResult::ok(out)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// ripgrep's content layout: `path:line:text` for a match, `path-line-text` for context,
/// `--` between groups that are not adjacent.
fn content_lines(hits: &[Hit], query: &Query, cwd: &Path, single_file: bool) -> Vec<String> {
    let mut out = Vec::new();
    let context = query.before > 0 || query.after > 0;
    let mut first_group = true;
    for hit in hits {
        let total = hit.lines.len();
        // Lines to show: the matches and their context, merged.
        let mut shown: BTreeSet<usize> = BTreeSet::new();
        for line in &hit.matched {
            let from = line.saturating_sub(query.before).max(1);
            let to = (line + query.after).min(total);
            shown.extend(from..=to);
        }
        let name = shown_path(&hit.path, cwd);
        let mut previous: Option<usize> = None;
        for line in shown {
            if context
                && (previous.is_some_and(|p| line > p + 1) || (previous.is_none() && !first_group))
            {
                out.push("--".to_owned());
            }
            previous = Some(line);
            let sep = if hit.matched.contains(&line) {
                ':'
            } else {
                '-'
            };
            let mut row = String::new();
            if !single_file {
                row.push_str(&name);
                row.push(sep);
            }
            if query.numbers {
                row.push_str(&line.to_string());
                row.push(sep);
            }
            row.push_str(&hit.lines[line - 1]);
            out.push(row);
        }
        first_group = false;
    }
    out
}

fn shown_path(path: &Path, cwd: &Path) -> String {
    shown(path, cwd)
}
