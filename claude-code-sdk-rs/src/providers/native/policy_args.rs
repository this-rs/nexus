//! What a policy pattern's argument is matched against, for the `nexus-tools` tools (N24).
//!
//! `Bash(git *)` and `Read(.env*)` are only as good as the string they are compared with. The
//! naive comparison — the pattern against the call's whole JSON, or the command line as one
//! string, or the path as the model wrote it — is bypassed by the first model that writes
//! `git status && rm -rf ~`, `./.env` or `src/../.env`. So:
//!
//! - **A command** is split into its simple commands at `;`, `&`, `|` and newlines outside
//!   quotes. A `deny` pattern that matches the whole line or **any** part denies the call; an
//!   `allow` decision needs **every** part allowed. A command with a substitution
//!   (`$(…)`, backticks, `<(…)`) cannot be analysed: it is never allowed by an argument
//!   pattern (it falls to the mode), and the commands inside the substitution are checked by
//!   the `deny` patterns as if they stood alone.
//! - **A path** is compared in its normal form: relative to the session directory with `.` and
//!   `..` removed. A `deny` pattern also tries the path as written and its last component (a
//!   pattern without a slash means "anywhere", as in a `.gitignore`); an `allow` pattern gets
//!   the normal form only, so `src/../.env` is not covered by `Edit(src/*)`.
//!
//! Limits, stated: this is lexical. A symlink named innocently that points at `.env` is judged
//! by its name; what confines the file tools is the scope of `nexus-tools` (which resolves
//! symlinks), not a pattern. A shell can always do what its user can: the policy decides what is
//! *asked*, it is not a sandbox.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::tools::ToolEntry;
use crate::agent::{PolicyDecision, ToolPolicy};

/// The strings a call is compared with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Forms {
    /// Any of these matched by a `deny` pattern denies the call.
    pub deny: Vec<String>,
    /// What an `allow` decision is made on.
    pub allow: AllowOn,
}

/// What an `allow` decision is made on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AllowOn {
    /// Each of these must be allowed (the parts of a command line; the one normal form of a path).
    Each(Vec<String>),
    /// There is no argument to match (a command with a substitution, or a missing field): only
    /// a pattern without an argument and the mode can allow it.
    NoArgument,
}

/// The decision for one call.
pub(crate) fn decide_call(
    policy: &ToolPolicy,
    approved: &HashSet<String>,
    entry: &ToolEntry,
    input: &Value,
    cwd: &Path,
) -> PolicyDecision {
    let forms = forms(entry, input, cwd);
    // Searching a file is reading it: `Read(.env*)` must stop `Grep` and `Glob` too.
    if matches!(entry.canonical.as_deref(), Some("Grep" | "Glob")) {
        for form in &forms.deny {
            if policy.decide("Read", Some(form), entry.category) == PolicyDecision::Deny {
                return PolicyDecision::Deny;
            }
        }
    }
    let mut allowed = false;
    let mut asked = false;
    for name in entry.names() {
        // Deny first: any form, any name.
        for form in &forms.deny {
            if policy.decide(name, Some(form), entry.category) == PolicyDecision::Deny {
                return PolicyDecision::Deny;
            }
        }
        let decision = match &forms.allow {
            AllowOn::NoArgument => policy.decide(name, None, entry.category),
            AllowOn::Each(each) if each.is_empty() => policy.decide(name, None, entry.category),
            AllowOn::Each(each) => {
                let mut all = PolicyDecision::Allow;
                for form in each {
                    match policy.decide(name, Some(form), entry.category) {
                        PolicyDecision::Deny => return PolicyDecision::Deny,
                        PolicyDecision::Ask => all = PolicyDecision::Ask,
                        PolicyDecision::Allow => {},
                    }
                }
                all
            },
        };
        match decision {
            PolicyDecision::Deny => return PolicyDecision::Deny,
            PolicyDecision::Allow => allowed = true,
            PolicyDecision::Ask => asked = true,
        }
    }
    if allowed {
        return PolicyDecision::Allow;
    }
    if asked && approved.contains(&entry.name) {
        return PolicyDecision::Allow;
    }
    PolicyDecision::Ask
}

fn forms(entry: &ToolEntry, input: &Value, cwd: &Path) -> Forms {
    let Some(canonical) = entry.canonical.as_deref() else {
        // A tool of another server: the whole input, as before.
        let whole = input.to_string();
        return Forms {
            deny: vec![whole.clone()],
            allow: AllowOn::Each(vec![whole]),
        };
    };
    let field = |name: &str| input.get(name).and_then(Value::as_str);
    match canonical {
        "Bash" | "Monitor" => match field("command") {
            Some(command) => command_forms(command),
            None => Forms {
                deny: Vec::new(),
                allow: AllowOn::NoArgument,
            },
        },
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            let given = field(if canonical == "NotebookEdit" {
                "notebook_path"
            } else {
                "file_path"
            });
            match given {
                Some(given) => path_forms(given, cwd),
                None => Forms {
                    deny: Vec::new(),
                    allow: AllowOn::NoArgument,
                },
            }
        },
        "Grep" | "Glob" => {
            let mut forms = path_forms(field("path").unwrap_or("."), cwd);
            // What is searched for in a file name counts too: `Glob(".env*")`, `Grep(glob: ".env*")`.
            for extra in [
                field("glob"),
                field("pattern").filter(|_| canonical == "Glob"),
            ]
            .into_iter()
            .flatten()
            {
                forms.deny.extend(path_forms(extra, cwd).deny);
            }
            // A directory searched is every file under it: `Read(secrets/*)` covers `secrets/`.
            let directories: Vec<String> = forms
                .deny
                .iter()
                .filter(|form| !form.ends_with('/') && form.as_str() != ".")
                .map(|form| format!("{form}/"))
                .collect();
            forms.deny.extend(directories);
            forms.allow = match entry.primary_argument(input) {
                Some(argument) => AllowOn::Each(vec![argument]),
                None => AllowOn::NoArgument,
            };
            dedupe(&mut forms.deny);
            forms
        },
        _ => match entry.primary_argument(input) {
            Some(argument) => Forms {
                deny: vec![argument.clone()],
                allow: AllowOn::Each(vec![argument]),
            },
            None => Forms {
                deny: Vec::new(),
                allow: AllowOn::NoArgument,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `path` lexically normalised: made relative to `cwd` when it is inside it, with `.` and `..`
/// resolved. `..` above the root of a relative path is kept (it is outside).
pub(crate) fn normalise(path: &str, cwd: &Path) -> String {
    // A leading `/` is rooted on every platform: `Path::is_absolute` says no on Windows (no
    // drive letter), and the path would then be glued to the session directory.
    let absolute = if Path::new(path).is_absolute() || path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            },
            Component::CurDir => {},
            other => out.push(other.as_os_str()),
        }
    }
    // The policy patterns use `/` whatever the platform: a `\` in the normal form would let
    // `Edit(secrets/*)` miss `secrets\key.pem` on Windows.
    // (On Unix a backslash is an ordinary filename character: leave it alone there.)
    let slashed = |p: &Path| {
        let shown = p.display().to_string();
        if cfg!(windows) {
            shown.replace('\\', "/")
        } else {
            shown
        }
    };
    match out.strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => slashed(relative),
        Err(_) => slashed(&out),
    }
}

fn path_forms(given: &str, cwd: &Path) -> Forms {
    let normal = normalise(given, cwd);
    let mut deny = vec![normal.clone(), given.to_owned()];
    if let Some(last) = Path::new(&normal).file_name().and_then(|n| n.to_str()) {
        deny.push(last.to_owned());
    }
    // A pattern may be written `./.env` or with the absolute path: offer those spellings of a
    // path that is inside the session directory.
    if normal != "."
        && !normal.starts_with("..")
        && !normal.starts_with('/')
        && !normal.contains(':')
    {
        deny.push(format!("./{normal}"));
        deny.push(slash(&cwd.join(&normal).display().to_string()));
    }
    // macOS and Windows file systems ignore case: `.ENV` is `.env`. Lower-case spellings are
    // offered too; denying a differently-cased name on Linux is harmless.
    let lowered: Vec<String> = deny.iter().map(|form| form.to_lowercase()).collect();
    deny.extend(lowered);
    dedupe(&mut deny);
    Forms {
        deny,
        allow: AllowOn::Each(vec![normal]),
    }
}

fn slash(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

/// Removes repeats, keeping the first of each.
fn dedupe(forms: &mut Vec<String>) {
    let mut seen = HashSet::new();
    forms.retain(|form| seen.insert(form.clone()));
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn command_forms(command: &str) -> Forms {
    let split = split_command(command);
    let mut deny = vec![command.trim().to_owned()];
    deny.extend(split.parts.iter().cloned());
    deny.extend(split.inner.iter().cloned());
    // What a part really runs: behind a keyword (`then`, `time`, `nohup`, `env`), an assignment
    // (`FOO=1 cmd`), a path (`/bin/rm`, `//bin/rm`) or a shell (`sh -c '…'`, `eval '…'`).
    let mut seen = Vec::new();
    for part in split.parts.iter().chain(split.inner.iter()) {
        unwrap_command(part, 0, &mut seen);
    }
    deny.extend(seen);
    dedupe(&mut deny);
    let allow = if split.substitution || split.parts.is_empty() {
        AllowOn::NoArgument
    } else {
        AllowOn::Each(split.parts)
    };
    Forms { deny, allow }
}

/// Words that put another command after them.
const WRAPPERS: &[&str] = &[
    "then", "do", "else", "elif", "if", "while", "until", "!", "time", "nohup", "exec", "command",
    "builtin", "env", "nice", "sudo", "doas", "setsid", "xargs", "ionice",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];
const MAX_UNWRAP_DEPTH: usize = 4;

/// One word of a command line: where it starts, and its text without quotes.
struct Word {
    start: usize,
    text: String,
}

fn words(line: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut current: Option<Word> = None;
    let mut quote: Option<char> = None;
    for (at, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current
                .get_or_insert(Word {
                    start: at,
                    text: String::new(),
                })
                .text
                .push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                current.get_or_insert(Word {
                    start: at,
                    text: String::new(),
                });
            },
            (None, c) if c.is_whitespace() => out.extend(current.take()),
            (None, c) => current
                .get_or_insert(Word {
                    start: at,
                    text: String::new(),
                })
                .text
                .push(c),
        }
    }
    out.extend(current);
    out
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn base_name(word: &str) -> &str {
    word.rsplit('/')
        .find(|piece| !piece.is_empty())
        .unwrap_or(word)
}

/// Pushes what `part` really runs, and, for a shell or `eval`, what it is told to run.
fn unwrap_command(part: &str, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_UNWRAP_DEPTH {
        return;
    }
    let words = words(part);
    let mut i = 0;
    while let Some(word) = words.get(i) {
        let base = base_name(&word.text);
        if WRAPPERS.contains(&base) {
            i += 1;
            // Its options: `time -p`, `env -i`, `nice -n 10`.
            while words.get(i).is_some_and(|w| w.text.starts_with('-')) {
                let takes_value = base == "nice" && words[i].text == "-n";
                i += if takes_value { 2 } else { 1 };
            }
        } else if is_assignment(&word.text) {
            i += 1;
        } else {
            break;
        }
    }
    let Some(command) = words.get(i) else { return };
    let base = base_name(&command.text).to_owned();
    // The command with its path reduced to a name: `//bin/rm -rf x` is `rm -rf x`.
    let rest = part[command.start..]
        .split_once(char::is_whitespace)
        .map_or(String::new(), |(_, rest)| format!(" {}", rest.trim_start()));
    out.push(format!("{base}{rest}"));

    let payload = if SHELLS.contains(&base.as_str()) {
        // `-c`, `-lc`, `-ec`: the next word is the script.
        let flag = words[i + 1..].iter().position(|w| {
            w.text.starts_with('-') && !w.text.starts_with("--") && w.text.contains('c')
        });
        flag.and_then(|at| words.get(i + 1 + at + 1))
            .map(|w| w.text.clone())
    } else if base == "eval" {
        Some(
            words[i + 1..]
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        None
    };
    if let Some(script) = payload {
        let inner = split_command(&script);
        for piece in inner.parts.iter().chain(inner.inner.iter()) {
            out.push(piece.clone());
            unwrap_command(piece, depth + 1, out);
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Split {
    /// The simple commands of the line, in order.
    pub parts: Vec<String>,
    /// Commands found inside substitutions, for the `deny` patterns.
    pub inner: Vec<String>,
    /// The line contains a substitution (`$(…)`, backticks, `<(…)`, `>(…)`).
    pub substitution: bool,
}

/// Splits a shell line at `;`, `&`, `|` and newlines that are outside quotes, and notes
/// substitutions. Not a shell parser: it is deliberately conservative — anything it cannot see
/// through makes the call fall back to "no argument to match", never to "allowed".
pub(crate) fn split_command(command: &str) -> Split {
    let mut split = Split::default();
    split_into(command, &mut split, true);
    split
}

fn split_into(text: &str, split: &mut Split, top: bool) {
    let chars: Vec<char> = text.chars().collect();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    let flush = |current: &mut String, split: &mut Split| {
        let part = current.trim().to_owned();
        current.clear();
        if !part.is_empty() {
            if top {
                split.parts.push(part);
            } else {
                split.inner.push(part);
            }
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some('\'') => {
                current.push(c);
                if c == '\'' {
                    quote = None;
                }
            },
            Some(q) => {
                // Inside double quotes a backslash escapes, and a substitution is still live.
                if c == '\\' && i + 1 < chars.len() {
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if c == '$' && chars.get(i + 1) == Some(&'(') {
                    i = substitution(&chars, i, split, &mut current);
                    continue;
                }
                if c == '`' {
                    i = backticks(&chars, i, split, &mut current);
                    continue;
                }
                current.push(c);
                if c == q {
                    quote = None;
                }
            },
            None => match c {
                '\\' if i + 1 < chars.len() => {
                    current.push(c);
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                },
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                },
                '$' if chars.get(i + 1) == Some(&'(') => {
                    i = substitution(&chars, i, split, &mut current);
                    continue;
                },
                '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                    i = substitution(&chars, i, split, &mut current);
                    continue;
                },
                '`' => {
                    i = backticks(&chars, i, split, &mut current);
                    continue;
                },
                ';' | '&' | '|' | '\n' | '\r' => flush(&mut current, split),
                '(' | ')' | '{' | '}' => {
                    // A subshell or a group: its content is commands too.
                    flush(&mut current, split);
                },
                _ => current.push(c),
            },
        }
        i += 1;
    }
    if quote.is_some() {
        // An unterminated quote: the shell would wait for more. Not analysable.
        split.substitution = true;
    }
    flush(&mut current, split);
}

/// `$(…)` or `<(…)` at `start`: records the inner commands, returns where to resume.
fn substitution(chars: &[char], start: usize, split: &mut Split, current: &mut String) -> usize {
    split.substitution = true;
    let open = start + 1; // the `(`
    let mut depth = 0usize;
    let mut end = chars.len();
    for (offset, c) in chars[open..].iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = open + offset;
                    break;
                }
            },
            _ => {},
        }
    }
    let inner: String = chars[open + 1..end.min(chars.len())].iter().collect();
    split_into(&inner, split, false);
    current.push(' ');
    (end + 1).min(chars.len())
}

fn backticks(chars: &[char], start: usize, split: &mut Split, current: &mut String) -> usize {
    split.substitution = true;
    let end = chars[start + 1..]
        .iter()
        .position(|c| *c == '`')
        .map_or(chars.len(), |p| start + 1 + p);
    let inner: String = chars[start + 1..end].iter().collect();
    split_into(&inner, split, false);
    current.push(' ');
    (end + 1).min(chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{PolicyMode, ToolPolicy};
    use serde_json::json;
    use std::path::Path;

    const CWD: &str = "/work/project";

    fn nexus(tool: &str) -> ToolEntry {
        ToolEntry::mcp(
            "nexus",
            tool,
            String::new(),
            json!({"type": "object"}),
            tool == "Read",
        )
    }

    fn policy(mode: PolicyMode, allow: &[&str], deny: &[&str]) -> ToolPolicy {
        ToolPolicy::from_patterns(mode, allow, deny).unwrap()
    }

    fn decide(policy: &ToolPolicy, tool: &str, input: Value) -> PolicyDecision {
        decide_call(
            policy,
            &HashSet::new(),
            &nexus(tool),
            &input,
            Path::new(CWD),
        )
    }

    fn bash(policy: &ToolPolicy, command: &str) -> PolicyDecision {
        decide(policy, "Bash", json!({"command": command}))
    }

    // ----- command splitting ------------------------------------------------

    fn parts(command: &str) -> Vec<String> {
        split_command(command).parts
    }

    #[test]
    fn a_line_is_split_at_every_command_separator_outside_quotes() {
        assert_eq!(parts("git status"), ["git status"]);
        assert_eq!(parts("git status && rm -rf x"), ["git status", "rm -rf x"]);
        assert_eq!(
            parts("a; b || c | d & e\nf"),
            ["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(parts("echo \"a; rm b\" ; ls"), ["echo \"a; rm b\"", "ls"]);
        assert_eq!(parts("echo 'a && b'"), ["echo 'a && b'"]);
        assert_eq!(parts("echo a\\;b"), ["echo a\\;b"]);
        assert_eq!(parts("(cd x && make)"), ["cd x", "make"]);
        assert_eq!(parts("{ a; b; }"), ["a", "b"]);
        assert!(parts("   ").is_empty());
    }

    #[test]
    fn substitutions_are_noticed_and_their_commands_are_extracted() {
        let s = split_command("echo $(rm -rf x)");
        assert!(s.substitution);
        assert_eq!(s.inner, ["rm -rf x"]);
        let s = split_command("echo `id; whoami`");
        assert!(s.substitution);
        assert_eq!(s.inner, ["id", "whoami"]);
        let s = split_command("diff <(ls a) <(ls b)");
        assert!(s.substitution);
        assert_eq!(s.inner, ["ls a", "ls b"]);
        // Live inside double quotes, dead inside single quotes.
        assert!(split_command("echo \"$(date)\"").substitution);
        assert!(!split_command("echo '$(date)'").substitution);
        // Nested.
        assert!(
            split_command("echo $(echo $(rm x))")
                .inner
                .contains(&"rm x".to_owned())
        );
        // An unterminated quote cannot be analysed.
        assert!(split_command("echo \"unterminated").substitution);
    }

    // ----- commands under a policy -----------------------------------------

    #[test]
    fn an_allowed_prefix_allows_only_what_it_covers() {
        let p = policy(PolicyMode::Ask, &["Bash(git *)"], &[]);
        assert_eq!(bash(&p, "git status"), PolicyDecision::Allow);
        assert_eq!(
            bash(&p, "git log --oneline | git shortlog"),
            PolicyDecision::Allow
        );
        assert_eq!(bash(&p, "cargo test"), PolicyDecision::Ask);
        // The bypasses: a second command rides on the allowed one.
        for sneaky in [
            "git status && rm -rf ~",
            "git status; rm -rf ~",
            "git status || rm -rf ~",
            "git status | sh",
            "git status & rm -rf ~",
            "git status\nrm -rf ~",
            "git status $(rm -rf ~)",
            "git status `rm -rf ~`",
            "git status <(rm -rf ~)",
            "git status \"unterminated",
            "(git status; rm -rf ~)",
        ] {
            assert_eq!(
                bash(&p, sneaky),
                PolicyDecision::Ask,
                "{sneaky:?} must not be allowed"
            );
        }
        // A quoted separator is not a separator.
        assert_eq!(
            bash(&p, "git commit -m \"fix a; b && c\""),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn a_denied_command_is_denied_wherever_it_hides() {
        let p = policy(PolicyMode::Ask, &["Bash(git *)"], &["Bash(rm *)"]);
        for hidden in [
            "rm -rf /",
            "git status && rm -rf /",
            "git status; rm x",
            "echo hi | rm x",
            "echo $(rm -rf /)",
            "echo `rm -rf /`",
            "diff <(rm x) b",
            "echo \"$(rm x)\"",
            "(cd x && rm y)",
            "ls\nrm y",
        ] {
            assert_eq!(
                bash(&p, hidden),
                PolicyDecision::Deny,
                "{hidden:?} must be denied"
            );
        }
        // Not a command: text inside quotes.
        assert_eq!(bash(&p, "echo \"rm x\""), PolicyDecision::Ask);
        assert_eq!(bash(&p, "echo 'rm x'"), PolicyDecision::Ask);
    }

    #[test]
    fn a_pattern_without_an_argument_allows_any_command_and_the_mode_still_rules() {
        let p = policy(PolicyMode::Ask, &["Bash"], &[]);
        assert_eq!(bash(&p, "anything && at all"), PolicyDecision::Allow);
        assert_eq!(bash(&p, "echo $(whatever)"), PolicyDecision::Allow);
        let ask = policy(PolicyMode::Ask, &[], &[]);
        assert_eq!(bash(&ask, "ls"), PolicyDecision::Ask);
        let plan = policy(PolicyMode::PlanOnly, &["Bash(*)"], &[]);
        assert_eq!(bash(&plan, "ls"), PolicyDecision::Deny);
        let auto = policy(PolicyMode::AutoEdits, &[], &[]);
        assert_eq!(
            bash(&auto, "ls"),
            PolicyDecision::Ask,
            "auto_edits does not run commands"
        );
    }

    #[test]
    fn the_full_tool_name_and_the_canonical_name_both_count() {
        let by_full = policy(PolicyMode::Ask, &["mcp__nexus__Bash(git *)"], &[]);
        // The full name's argument pattern is matched against the same command forms.
        assert_eq!(bash(&by_full, "git status"), PolicyDecision::Allow);
        let deny_full = policy(PolicyMode::Ask, &[], &["mcp__nexus__Bash(rm *)"]);
        assert_eq!(bash(&deny_full, "ls; rm x"), PolicyDecision::Deny);
        let deny_canonical = policy(PolicyMode::Ask, &[], &["Bash"]);
        assert_eq!(bash(&deny_canonical, "ls"), PolicyDecision::Deny);
    }

    // ----- paths ------------------------------------------------------------

    // Windows paths: the normal form uses `/` like the policy patterns, and a rooted `/x/y`
    // is not glued to the session directory.
    #[cfg(windows)]
    #[test]
    fn a_windows_path_is_normalised_to_forward_slashes() {
        let cwd = Path::new(r"C:\work\project");
        assert_eq!(normalise(r"secrets\key.pem", cwd), "secrets/key.pem");
        assert_eq!(
            normalise(r"C:\work\project\secrets\key.pem", cwd),
            "secrets/key.pem"
        );
        assert_eq!(normalise(r"src\..\.env", cwd), ".env");
        assert_eq!(normalise(r"C:\work\project", cwd), ".");
    }

    // POSIX paths (`/work/project`): not absolute on Windows, where the harness paths are untested.
    #[cfg(unix)]
    #[test]
    fn a_path_is_normalised_against_the_session_directory() {
        let n = |p: &str| normalise(p, Path::new(CWD));
        assert_eq!(n(".env"), ".env");
        assert_eq!(n("./.env"), ".env");
        assert_eq!(n("src/../.env"), ".env");
        assert_eq!(n("/work/project/.env"), ".env");
        assert_eq!(n("/work/project/src/../src/a.rs"), "src/a.rs");
        assert_eq!(n("."), ".");
        assert_eq!(n("/work/project"), ".");
        assert_eq!(n("../elsewhere/x"), "/work/elsewhere/x");
        assert_eq!(n("/etc/passwd"), "/etc/passwd");
        assert_eq!(n("a//b/./c"), "a/b/c");
    }

    // POSIX paths (`/work/project`): not absolute on Windows, where the harness paths are untested.
    #[cfg(unix)]
    #[test]
    fn a_denied_path_is_denied_however_it_is_written() {
        let p = policy(PolicyMode::Ask, &[], &["Read(.env*)", "Edit(secrets/*)"]);
        for path in [
            ".env",
            ".env.local",
            "./.env",
            "src/../.env",
            "/work/project/.env",
            "/work/project/src/../.env.production",
            "sub/dir/.env", // a pattern with no slash means "anywhere"
            "/work/project/sub/.env",
        ] {
            assert_eq!(
                decide(&p, "Read", json!({"file_path": path})),
                PolicyDecision::Deny,
                "{path} must be denied"
            );
        }
        assert_eq!(
            decide(&p, "Read", json!({"file_path": "docs/environment.md"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, "Read", json!({"file_path": "src/main.rs"})),
            PolicyDecision::Allow
        );
        for path in [
            "secrets/key.pem",
            "./secrets/key.pem",
            "x/../secrets/a/b",
            "/work/project/secrets/k",
        ] {
            assert_eq!(
                decide(&p, "Edit", json!({"file_path": path})),
                PolicyDecision::Deny,
                "{path} must be denied"
            );
        }
    }

    // POSIX paths (`/work/project`): not absolute on Windows, where the harness paths are untested.
    #[cfg(unix)]
    #[test]
    fn an_allowed_path_covers_only_its_normal_form() {
        let p = policy(PolicyMode::Ask, &["Edit(src/*)"], &[]);
        assert_eq!(
            decide(&p, "Edit", json!({"file_path": "src/a.rs"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&p, "Edit", json!({"file_path": "/work/project/src/a.rs"})),
            PolicyDecision::Allow
        );
        // The raw string matches `src/*`; the real path is .env.
        assert_eq!(
            decide(&p, "Edit", json!({"file_path": "src/../.env"})),
            PolicyDecision::Ask
        );
        assert_eq!(
            decide(&p, "Edit", json!({"file_path": "docs/a.md"})),
            PolicyDecision::Ask
        );
        assert_eq!(
            decide(&p, "Write", json!({"file_path": "src/a.rs"})),
            PolicyDecision::Ask,
            "Edit(...) is not Write(...)"
        );
    }

    #[test]
    fn edits_pass_in_auto_edits_commands_do_not_and_reads_never_ask() {
        let auto = policy(PolicyMode::AutoEdits, &[], &[]);
        assert_eq!(
            decide(&auto, "Edit", json!({"file_path": "a.rs"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&auto, "NotebookEdit", json!({"notebook_path": "a.ipynb"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&auto, "Read", json!({"file_path": "a.rs"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(&auto, "Grep", json!({"pattern": "x"})),
            PolicyDecision::Allow
        );
        assert_eq!(bash(&auto, "ls"), PolicyDecision::Ask);
        let ask = policy(PolicyMode::Ask, &[], &[]);
        assert_eq!(
            decide(&ask, "Edit", json!({"file_path": "a.rs"})),
            PolicyDecision::Ask
        );
        assert_eq!(
            decide(&ask, "Read", json!({"file_path": "a.rs"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn web_tools_ask_by_default_and_are_matched_by_domain() {
        let ask = policy(PolicyMode::Ask, &[], &[]);
        assert_eq!(
            decide(&ask, "WebFetch", json!({"url": "https://example.com/a"})),
            PolicyDecision::Ask
        );
        let allow = policy(PolicyMode::Ask, &["WebFetch(domain:docs.rs)"], &[]);
        assert_eq!(
            decide(&allow, "WebFetch", json!({"url": "https://docs.rs/serde"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            decide(
                &allow,
                "WebFetch",
                json!({"url": "https://evil.test/docs.rs"})
            ),
            PolicyDecision::Ask
        );
        let deny = policy(PolicyMode::Ask, &[], &["WebFetch(domain:evil.test)"]);
        assert_eq!(
            decide(&deny, "WebFetch", json!({"url": "https://evil.test/x"})),
            PolicyDecision::Deny
        );
        // No parsable URL: a `deny` with an argument matches an argument it cannot read
        // (fail closed, decision A35), and an `allow` with an argument never does.
        assert_eq!(
            decide(&deny, "WebFetch", json!({"url": "not a url"})),
            PolicyDecision::Deny
        );
        assert_eq!(
            decide(&allow, "WebFetch", json!({"url": "not a url"})),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn a_call_with_a_missing_field_is_never_allowed_by_an_argument_pattern() {
        let p = policy(PolicyMode::Ask, &["Bash(git *)", "Edit(src/*)"], &[]);
        assert_eq!(decide(&p, "Bash", json!({})), PolicyDecision::Ask);
        assert_eq!(decide(&p, "Edit", json!({})), PolicyDecision::Ask);
        assert_eq!(
            decide(&p, "Bash", json!({"command": 3})),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn a_tool_of_another_server_keeps_the_whole_input_treatment_and_never_inherits_a_canonical_name()
     {
        // A server called `evil` that offers a tool named `Read`: not `nexus-tools`, no canonical
        // name, so `Read(...)` patterns do not apply to it and it is not trusted as a read.
        let evil = ToolEntry::mcp("evil", "Read", String::new(), json!({}), false);
        assert_eq!(evil.canonical, None);
        let p = policy(PolicyMode::Ask, &["Read"], &["Read(.env*)"]);
        let decision = decide_call(
            &p,
            &HashSet::new(),
            &evil,
            &json!({"file_path": ".env"}),
            Path::new(CWD),
        );
        assert_eq!(
            decision,
            PolicyDecision::Ask,
            "only mcp__evil__Read patterns apply to it"
        );
        let own = policy(PolicyMode::Ask, &["mcp__evil__Read"], &[]);
        assert_eq!(
            decide_call(&own, &HashSet::new(), &evil, &json!({}), Path::new(CWD)),
            PolicyDecision::Allow
        );
    }

    // ----- bypasses found by the independent verification (N26) -------------

    #[cfg(unix)]
    #[test]
    fn a_deny_pattern_written_with_an_absolute_path_or_a_dot_slash_still_denies() {
        for pattern in ["Read(/work/project/.env)", "Read(./.env)", "Read(.env)"] {
            let p = policy(PolicyMode::Ask, &[], &[pattern]);
            for path in [".env", "./.env", "/work/project/.env", "src/../.env"] {
                assert_eq!(
                    decide(&p, "Read", json!({"file_path": path})),
                    PolicyDecision::Deny,
                    "{pattern} must deny {path}"
                );
            }
        }
    }

    #[test]
    fn a_denied_file_is_denied_whatever_its_case() {
        // macOS and Windows file systems are case-insensitive: `.ENV` is `.env` there.
        let p = policy(PolicyMode::Ask, &[], &["Read(.env*)", "Edit(secrets/*)"]);
        assert_eq!(
            decide(&p, "Read", json!({"file_path": ".ENV"})),
            PolicyDecision::Deny
        );
        assert_eq!(
            decide(&p, "Read", json!({"file_path": "x/.Env.Local"})),
            PolicyDecision::Deny
        );
        assert_eq!(
            decide(&p, "Edit", json!({"file_path": "Secrets/k.pem"})),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn a_denied_domain_is_denied_with_a_trailing_dot_or_capitals() {
        let p = policy(PolicyMode::Ask, &[], &["WebFetch(domain:evil.com)"]);
        for url in [
            "https://evil.com/x",
            "https://evil.com./x",
            "https://EVIL.com/x",
            "https://evil.com.:8443/x",
        ] {
            assert_eq!(
                decide(&p, "WebFetch", json!({"url": url})),
                PolicyDecision::Deny,
                "{url} must be denied"
            );
        }
    }

    #[test]
    fn grep_and_glob_obey_the_read_deny_patterns() {
        let p = policy(PolicyMode::Ask, &[], &["Read(.env*)", "Read(secrets/*)"]);
        for (tool, input) in [
            ("Grep", json!({"pattern": "KEY", "path": ".env"})),
            ("Grep", json!({"pattern": "KEY", "path": "secrets"})),
            (
                "Grep",
                json!({"pattern": "KEY", "path": "/work/project/secrets/a"}),
            ),
            ("Grep", json!({"pattern": "KEY", "glob": ".env*"})),
            ("Glob", json!({"pattern": ".env*"})),
            ("Glob", json!({"pattern": "*", "path": "secrets"})),
        ] {
            assert_eq!(
                decide(&p, tool, input.clone()),
                PolicyDecision::Deny,
                "{tool} {input}"
            );
        }
        assert_eq!(
            decide(&p, "Grep", json!({"pattern": "fn main", "path": "src"})),
            PolicyDecision::Allow,
            "a search that touches no denied path is still free"
        );
    }

    #[test]
    fn a_denied_command_is_denied_behind_every_wrapper() {
        let p = policy(PolicyMode::Ask, &[], &["Bash(rm *)"]);
        for command in [
            "if true; then rm -rf x; fi",
            "time rm -rf x",
            "nohup rm -rf x",
            "FOO=1 rm -rf x",
            "FOO=1 BAR=2 rm -rf x",
            "env FOO=1 rm -rf x",
            "/bin/rm -rf x",
            "//bin/rm -rf x",
            "/usr/bin/env rm -rf x",
            "command rm -rf x",
            "exec rm -rf x",
            "sh -c 'rm -rf x'",
            "bash -lc \"rm -rf x\"",
            "eval 'rm -rf x'",
            "sh -c 'sh -c \"rm -rf x\"'",
            "while true; do rm -rf x; done",
            "! rm -rf x",
        ] {
            assert_eq!(
                bash(&p, command),
                PolicyDecision::Deny,
                "{command} must be denied"
            );
        }
    }

    #[test]
    fn those_wrappers_never_make_a_command_allowed() {
        let p = policy(PolicyMode::Ask, &["Bash(git *)"], &[]);
        assert_eq!(bash(&p, "git status"), PolicyDecision::Allow);
        for command in [
            "sh -c 'git status'",
            "eval 'git status'",
            "FOO=1 git status",
            "time git status",
        ] {
            assert_eq!(
                bash(&p, command),
                PolicyDecision::Ask,
                "{command} is not a plain git command"
            );
        }
    }

    #[test]
    fn a_deny_pattern_that_is_the_literal_text_the_model_wrote_still_denies() {
        // The pattern spells the path as it was written, `..` included. Its normal form is `.env`
        // and its last component is `.env`: only the as-written form can match.
        let p = policy(PolicyMode::Ask, &[], &["Read(src/../.env)"]);
        assert_eq!(
            decide(&p, "Read", json!({"file_path": "src/../.env"})),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn a_session_approval_lifts_an_ask_but_never_a_deny() {
        let p = policy(PolicyMode::Ask, &[], &["Bash(rm *)"]);
        let approved: HashSet<String> = ["mcp__nexus__Bash".to_owned()].into();
        let run = |command: &str| {
            decide_call(
                &p,
                &approved,
                &nexus("Bash"),
                &json!({"command": command}),
                Path::new(CWD),
            )
        };
        assert_eq!(run("ls"), PolicyDecision::Allow);
        assert_eq!(run("ls; rm x"), PolicyDecision::Deny);
    }
}
