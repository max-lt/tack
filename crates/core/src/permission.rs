use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::sandbox::FileChange;

/// Describes a tool invocation that requires permission.
#[non_exhaustive]
pub enum Tool<'a> {
    Bash { command: &'a str },
    /// Apply the files a sandboxed command changed to the workspace.
    ApplyChanges {
        command: &'a str,
        changes: &'a [FileChange],
    },
    Read { path: &'a Path },
    Write { path: &'a Path },
    Edit { path: &'a Path },
    Fetch { url: &'a str, method: &'a str },
    Git { subcommand: &'a str },
    /// Start a command under a PTY. `host` asks to run it outside the
    /// sandbox.
    TerminalOpen { command: &'a str, host: bool },
    /// Type into an open terminal: it can run anything the terminal runs.
    /// `host`: the terminal runs on the user's machine. Unknown terminals
    /// count as on the host.
    TerminalSend { input: &'a str, host: bool },
    /// Read or close an open terminal.
    TerminalWatch,
    /// Start a background command whose output wakes the model. `host`
    /// asks to run it outside the sandbox.
    Monitor {
        command: &'a str,
        description: &'a str,
        host: bool,
    },
    /// List or stop monitors.
    MonitorWatch,
    /// Send to, pair with or drop a crosstalk peer: it leaves the machine.
    /// `text` is the message, or the description sent with a pairing.
    Crosstalk {
        action: &'a str,
        peer: &'a str,
        text: &'a str,
    },
    /// Read this session's crosstalk id or peers.
    CrosstalkWatch,
    Glob,
    Grep,
    List,
    Search,
}

/// Determines whether a given tool invocation is allowed.
///
/// `&mut self` allows stateful handlers (caching decisions, counters, etc.).
pub trait PermissionHandler: Send {
    fn allow(&mut self, tool: &Tool<'_>) -> bool;
}

/// Permits every tool invocation.
pub struct AllowAll;

impl PermissionHandler for AllowAll {
    fn allow(&mut self, _tool: &Tool<'_>) -> bool {
        true
    }
}

/// Denies every tool invocation.
pub struct DenyAll;

impl PermissionHandler for DenyAll {
    fn allow(&mut self, _tool: &Tool<'_>) -> bool {
        false
    }
}

impl PermissionHandler for Box<dyn PermissionHandler> {
    fn allow(&mut self, tool: &Tool<'_>) -> bool {
        (**self).allow(tool)
    }
}

// ---------------------------------------------------------------------------
// Rule-based permission configuration
// ---------------------------------------------------------------------------

/// Permission settings matching the Claude Code `.claude/settings.json` format.
///
/// ```json
/// {
///   "permissions": {
///     "allow": ["Bash(psql:*)", "Bash(find:*)"],
///     "deny": [],
///     "additionalDirectories": ["/extra/path"]
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionConfig {
    #[serde(default)]
    pub allow: Vec<String>,

    #[serde(default)]
    pub deny: Vec<String>,

    #[serde(default, rename = "additionalDirectories")]
    pub additional_directories: Vec<PathBuf>,
}

impl PermissionConfig {
    /// Check if a tool invocation is auto-allowed by the configured rules.
    ///
    /// Returns `Some(true)` if explicitly allowed, `Some(false)` if explicitly
    /// denied, or `None` if no rule matches (caller should prompt).
    pub fn check(&self, tool: &Tool<'_>, project_dir: &Path) -> Option<bool> {
        // Deny rules take precedence
        if self.deny.iter().any(|r| rule_matches(r, tool)) {
            return Some(false);
        }

        // Bash, Monitor and TerminalOpen run their command with bash. A
        // deny rule of the tool, or of Bash, covers each command of a chain.
        if let Tool::Bash { command }
        | Tool::Monitor { command, .. }
        | Tool::TerminalOpen { command, .. } = tool
        {
            for part in commands_in(command) {
                let bash = Tool::Bash { command: part };
                let same = with_command(tool, part);

                if self
                    .deny
                    .iter()
                    .any(|r| rule_matches(r, &bash) || rule_matches(r, &same))
                {
                    return Some(false);
                }
            }
        }

        // Text typed into a terminal runs as shell lines. A key token, such
        // as <Tab> or <C-m>, can start or change a line: split there too.
        if let Tool::TerminalSend { input, host } = tool {
            for part in typed_parts(input).into_iter().flat_map(commands_in) {
                let bash = Tool::Bash { command: part };
                let typed = Tool::TerminalSend {
                    input: part,
                    host: *host,
                };

                if self
                    .deny
                    .iter()
                    .any(|r| rule_matches(r, &bash) || rule_matches(r, &typed))
                {
                    return Some(false);
                }
            }

            // Text typed into a host terminal runs on the host: no allow
            // rule covers it, as for TerminalOpen with host true.
            if !host && self.typed_lines_allowed(input) {
                return Some(true);
            }
        }

        // Changes from a sandbox are a set of writes: a deny rule on any of
        // them refuses the set. No rule allows them without asking.
        if let Tool::ApplyChanges { changes, .. } = tool {
            let denied = changes.iter().any(|change| {
                let path = change.path();
                let write = Tool::Write { path };
                self.deny.iter().any(|r| rule_matches(r, &write))
            });

            return denied.then_some(false);
        }

        // A run on the host leaves the sandbox. Rules are written for the
        // sandbox, so no allow rule covers it: the user is asked.
        let on_host = matches!(
            tool,
            Tool::Monitor { host: true, .. }
                | Tool::TerminalOpen { host: true, .. }
                | Tool::TerminalSend { host: true, .. }
        );

        // Check explicit allow rules
        if !on_host && self.allow.iter().any(|r| allow_matches(r, tool)) {
            return Some(true);
        }

        // Read-only tools are always allowed
        match tool {
            Tool::Glob | Tool::Grep | Tool::List | Tool::Search | Tool::TerminalWatch | Tool::MonitorWatch | Tool::CrosstalkWatch => {
                return Some(true);
            }
            Tool::Git { subcommand } => {
                // Read-only git commands are auto-allowed
                if is_readonly_git_command(subcommand) {
                    return Some(true);
                }
            }
            _ => {}
        }

        // File operations in allowed directories are auto-allowed
        match tool {
            Tool::Read { path } | Tool::Write { path } | Tool::Edit { path } => {
                let resolved = resolve_path(path, project_dir);

                if resolved.starts_with(project_dir) {
                    return Some(true);
                }

                if self
                    .additional_directories
                    .iter()
                    .any(|dir| resolved.starts_with(dir))
                {
                    return Some(true);
                }
            }
            _ => {}
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Rule parsing and matching
// ---------------------------------------------------------------------------

/// Characters that make bash run more than one command, or write or read
/// a file: `;`, `&`, `|`, `$(...)`, backquotes, subshells, redirects and
/// newlines.
const SHELL_METACHARACTERS: &[char] = &[';', '&', '|', '`', '$', '(', ')', '<', '>', '\n', '\r'];

/// The commands in a bash command line: the parts between `;`, `&`, `|`,
/// newlines, backquotes and brackets (so `$(...)` too), without the
/// variable assignments in front, as in `FOO=1 rm x`. It does not parse
/// quotes: it finds more parts than bash, never fewer, which suits deny
/// rules.
fn commands_in(line: &str) -> Vec<&str> {
    line.split([';', '&', '|', '\n', '\r', '`', '(', ')'])
        .map(|part| {
            let mut rest = part.trim_start();

            while let Some((word, after)) = rest.split_once(char::is_whitespace)
                && is_assignment(word)
            {
                rest = after.trim_start();
            }

            rest.trim_end()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

/// `NAME=value`, a variable assignment before a command.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `tool` with `command` in the place of its own command.
fn with_command<'a>(tool: &Tool<'a>, command: &'a str) -> Tool<'a> {
    match *tool {
        Tool::Monitor {
            description, host, ..
        } => Tool::Monitor {
            command,
            description,
            host,
        },
        Tool::TerminalOpen { host, .. } => Tool::TerminalOpen { command, host },
        _ => Tool::Bash { command },
    }
}

/// Text and lines of TerminalSend input, split at each key token such as
/// `<Enter>` or `<Tab>` and at each line break.
fn typed_parts(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find('<') {
        parts.push(&rest[..start]);
        rest = match rest[start..].find('>') {
            Some(end) => &rest[start + end + 1..],
            None => &rest[start + 1..],
        };
    }
    parts.push(rest);

    parts
        .into_iter()
        .flat_map(|part| part.split(['\n', '\r']))
        .filter(|part| !part.trim().is_empty())
        .collect()
}

impl PermissionConfig {
    /// Whether prefix rules allow each line of TerminalSend input. The
    /// lines are the text between `<Enter>` and line breaks. Another key
    /// token stays in its line, and its `<` makes the line ask: `<Up>` or
    /// `<Tab>` can make the shell run a line that is not the text.
    fn typed_lines_allowed(&self, input: &str) -> bool {
        let lines: Vec<&str> = input
            .split("<Enter>")
            .flat_map(|chunk| chunk.split(['\n', '\r']))
            .filter(|line| !line.trim().is_empty())
            .collect();

        !lines.is_empty()
            && lines.iter().all(|line| {
                !line.contains(SHELL_METACHARACTERS)
                    && self.allow.iter().any(|rule| {
                        let typed = Tool::TerminalSend {
                            input: line,
                            host: false,
                        };
                        is_prefix_rule(rule) && rule_matches(rule, &typed)
                    })
            })
    }
}

fn is_prefix_rule(rule: &str) -> bool {
    parse_rule(rule).is_some_and(|(_, pattern)| pattern.ends_with(":*"))
}

/// `rule_matches` for an allow rule. A prefix rule such as `Bash(tail:*)`
/// names one command; it does not cover a command with a shell
/// metacharacter, such as `tail x; rm -rf ~`. That command is asked.
fn allow_matches(rule: &str, tool: &Tool<'_>) -> bool {
    let command = match tool {
        Tool::Bash { command }
        | Tool::Monitor { command, .. }
        | Tool::TerminalOpen { command, .. } => Some(*command),
        // Prefix rules for typed text are checked line by line, in
        // `typed_lines_allowed`.
        Tool::TerminalSend { .. } if is_prefix_rule(rule) => return false,
        _ => None,
    };

    if is_prefix_rule(rule) && command.is_some_and(|c| c.contains(SHELL_METACHARACTERS)) {
        return false;
    }

    rule_matches(rule, tool)
}

/// Parse a rule string like `Bash(psql:*)` and check if it matches a tool.
fn rule_matches(rule: &str, tool: &Tool<'_>) -> bool {
    let Some((tool_name, pattern)) = parse_rule(rule) else {
        return false;
    };

    match (tool_name, tool) {
        ("Bash", Tool::Bash { command }) => pattern_matches(command, pattern),
        ("Read", Tool::Read { path }) => pattern_matches(&path.display().to_string(), pattern),
        ("Write", Tool::Write { path }) => pattern_matches(&path.display().to_string(), pattern),
        ("Edit", Tool::Edit { path }) => pattern_matches(&path.display().to_string(), pattern),
        ("Git", Tool::Git { subcommand }) => pattern_matches(subcommand, pattern),
        ("TerminalOpen", Tool::TerminalOpen { command, .. }) => pattern_matches(command, pattern),
        ("TerminalSend", Tool::TerminalSend { input, .. }) => pattern_matches(input, pattern),
        ("Monitor", Tool::Monitor { command, .. }) => pattern_matches(command, pattern),
        ("Crosstalk", Tool::Crosstalk { action, peer, .. }) => {
            pattern_matches(&format!("{action} {peer}"), pattern)
        }
        _ => false,
    }
}

/// Extract tool name and pattern from `ToolName(pattern)`.
fn parse_rule(rule: &str) -> Option<(&str, &str)> {
    let open = rule.find('(')?;
    let close = rule.rfind(')')?;

    if close <= open {
        return None;
    }

    Some((&rule[..open], &rule[open + 1..close]))
}

/// Match a value against a pattern.
///
/// - `*` matches everything.
/// - `prefix:*` matches if `value` equals `prefix` or starts with `prefix `
///   (prefix followed by a space — i.e. the prefix is the command/path start).
/// - Anything else is an exact match.
fn pattern_matches(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if let Some(prefix) = pattern.strip_suffix(":*") {
        // An empty prefix would match each value that starts with a space.
        if prefix.is_empty() {
            return false;
        }

        return value == prefix
            || value.starts_with(prefix)
                && value
                    .as_bytes()
                    .get(prefix.len())
                    .is_some_and(|&b| b == b' ');
    }

    value == pattern
}

/// Check if a Git subcommand is read-only.
fn is_readonly_git_command(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "status" | "diff_staged" | "diff_unstaged" | "diff" | "log" | "show" | "blame" | "branch"
    )
}

/// Resolve a potentially relative path against the project directory.
fn resolve_path(path: &Path, project_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rule() {
        assert_eq!(parse_rule("Bash(psql:*)"), Some(("Bash", "psql:*")));
        assert_eq!(parse_rule("Bash(*)"), Some(("Bash", "*")));
        assert_eq!(
            parse_rule("Bash(bun scripts/generate-types.ts:*)"),
            Some(("Bash", "bun scripts/generate-types.ts:*"))
        );
        assert_eq!(parse_rule("invalid"), None);
        assert_eq!(parse_rule("no_parens"), None);
    }

    #[test]
    fn test_pattern_matches_wildcard() {
        assert!(pattern_matches("anything", "*"));
        assert!(pattern_matches("", "*"));
    }

    #[test]
    fn test_pattern_matches_prefix() {
        assert!(pattern_matches("psql", "psql:*"));
        assert!(pattern_matches("psql -U admin mydb", "psql:*"));
        assert!(!pattern_matches("psql2", "psql:*"));
        assert!(!pattern_matches("xpsql", "psql:*"));
    }

    #[test]
    fn test_pattern_matches_multi_word_prefix() {
        let pat = "bun scripts/generate-types.ts:*";
        assert!(pattern_matches("bun scripts/generate-types.ts", pat));
        assert!(pattern_matches("bun scripts/generate-types.ts --flag", pat));
        assert!(!pattern_matches("bun scripts/generate-types.tsx", pat));
    }

    #[test]
    fn test_pattern_matches_exact() {
        assert!(pattern_matches("exact", "exact"));
        assert!(!pattern_matches("exact2", "exact"));
    }

    fn config(allow: &[&str], deny: &[&str]) -> PermissionConfig {
        PermissionConfig {
            allow: allow.iter().map(|r| r.to_string()).collect(),
            deny: deny.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn an_allow_rule_never_covers_a_run_on_the_host() {
        let c = config(&["Monitor(tail:*)", "TerminalOpen(psql:*)"], &[]);
        let p = Path::new("/project");
        let monitor = |host| Tool::Monitor { command: "tail -n 200 ~/.aws/credentials", description: "d", host };
        let terminal = |host| Tool::TerminalOpen { command: "psql prod", host };

        assert_eq!(c.check(&monitor(false), p), Some(true));
        assert_eq!(c.check(&monitor(true), p), None);
        assert_eq!(c.check(&terminal(false), p), Some(true));
        assert_eq!(c.check(&terminal(true), p), None);
    }

    #[test]
    fn a_bash_deny_rule_denies_the_command_in_monitor_and_terminal_open() {
        let c = config(&["Monitor(*)", "TerminalOpen(*)"], &["Bash(rm:*)"]);
        let p = Path::new("/project");

        for host in [false, true] {
            let monitor = Tool::Monitor { command: "rm -rf build", description: "d", host };
            let terminal = Tool::TerminalOpen { command: "rm -rf build", host };
            assert_eq!(c.check(&monitor, p), Some(false), "monitor host={host}");
            assert_eq!(c.check(&terminal, p), Some(false), "terminal host={host}");
        }

        let safe = Tool::Monitor { command: "tail -F log", description: "d", host: false };
        assert_eq!(c.check(&safe, p), Some(true));
    }

    /// Commands that start like `tail` but run more than tail.
    const CHAINED: &[&str] = &[
        "tail x; rm -rf ~",
        "tail x && rm y",
        "tail x || rm y",
        "tail x | sh",
        "tail x & rm y",
        "tail $(rm y)",
        "tail `rm y`",
        "tail x\nrm y",
        "tail x > ~/.bashrc",
        "tail x >> ~/.bashrc",
        "tail x < /etc/passwd",
    ];

    #[test]
    fn a_prefix_allow_rule_does_not_cover_a_chained_command() {
        let c = config(&["Bash(tail:*)", "Monitor(tail:*)", "TerminalOpen(tail:*)"], &[]);
        let p = Path::new("/project");

        assert_eq!(c.check(&Tool::Bash { command: "tail -n 5 log" }, p), Some(true));

        for command in CHAINED {
            let tools = [
                Tool::Bash { command },
                Tool::Monitor { command, description: "d", host: false },
                Tool::TerminalOpen { command, host: false },
            ];
            for tool in &tools {
                assert_eq!(c.check(tool, p), None, "{command:?} was allowed");
            }
        }
    }

    #[test]
    fn a_bash_deny_rule_finds_the_command_inside_a_chain() {
        let c = config(&["Bash(*)", "Monitor(*)", "TerminalOpen(*)"], &["Bash(rm:*)"]);
        let p = Path::new("/project");

        for command in CHAINED.iter().filter(|c| c.contains("rm")) {
            let tools = [
                Tool::Bash { command },
                Tool::Monitor { command, description: "d", host: false },
                Tool::TerminalOpen { command, host: false },
            ];
            for tool in &tools {
                assert_eq!(c.check(tool, p), Some(false), "{command:?} was not denied");
            }
        }

        // Variable assignments before the command do not hide it.
        assert_eq!(c.check(&Tool::Bash { command: "FOO=1 rm -rf x" }, p), Some(false));
        assert_eq!(c.check(&Tool::Bash { command: "ls; FOO=1 BAR=2 rm x" }, p), Some(false));
    }

    #[test]
    fn a_prefix_allow_rule_checks_each_line_typed_into_a_terminal() {
        let c = config(&["TerminalSend(ls:*)"], &[]);
        let p = Path::new("/project");
        let send = |input| c.check(&Tool::TerminalSend { input, host: false }, p);

        assert_eq!(send("ls -la<Enter>"), Some(true));
        assert_eq!(send("ls<Enter>ls -a<Enter>"), Some(true));

        for input in [
            "ls; rm -rf ~<Enter>",
            "ls | sh<Enter>",
            "ls $(rm y)<Enter>",
            "ls<Enter>rm -rf ~<Enter>",
            "ls\nrm -rf ~",
            "ls\rrm -rf ~",
            "ls<C-m>rm -rf ~<Enter>", // C-m is a second Enter
            "ls<Up><Enter>",          // history: the line that runs is not the text
            "ls<Tab><Enter>",
            "ls<Foo><Enter>",         // unknown token
        ] {
            assert_eq!(send(input), None, "{input:?} was allowed");
        }

        // The wildcard allows any input; an exact rule matches as written.
        let all = config(&["TerminalSend(*)"], &[]);
        assert_eq!(all.check(&Tool::TerminalSend { input: "ls<Up><Enter>", host: false }, p), Some(true));
    }

    #[test]
    fn an_allow_rule_never_covers_text_typed_into_a_host_terminal() {
        let c = config(&["TerminalSend(ls:*)", "TerminalSend(*)"], &["Bash(rm:*)"]);
        let p = Path::new("/project");
        let send = |input, host| c.check(&Tool::TerminalSend { input, host }, p);

        assert_eq!(send("ls<Enter>", false), Some(true));
        assert_eq!(send("ls<Enter>", true), None);
        assert_eq!(send("rm x<Enter>", true), Some(false));
    }

    #[test]
    fn a_deny_rule_checks_each_line_typed_into_a_terminal() {
        let c = config(&["TerminalSend(*)"], &["Bash(rm:*)"]);
        let p = Path::new("/project");
        let send = |input| c.check(&Tool::TerminalSend { input, host: false }, p);

        for input in [
            "rm -rf ~<Enter>",
            "ls<Enter>rm -rf ~<Enter>",
            "echo hi; rm x<Enter>",
            "ls<Tab>rm x",
            "ls<C-j>FOO=1 rm x<Enter>",
            "ls\nrm x",
        ] {
            assert_eq!(send(input), Some(false), "{input:?} was not denied");
        }

        assert_eq!(send("ls -la<Enter>"), Some(true));
    }

    #[test]
    fn deny_wins_over_allow_in_any_order() {
        let p = Path::new("/project");
        let ls = Tool::Bash { command: "ls -la" };

        for (allow, deny) in [(&["Bash(ls:*)", "Bash(*)"][..], &["Bash(ls:*)"][..]), (&["Bash(*)"][..], &["Bash(*)", "Bash(ls:*)"][..])] {
            assert_eq!(config(allow, deny).check(&ls, p), Some(false));
            let mut reversed = allow.to_vec();
            reversed.reverse();
            assert_eq!(config(&reversed, deny).check(&ls, p), Some(false));
        }
    }

    #[test]
    fn rules_that_do_not_parse_or_do_not_fit_match_nothing() {
        let p = Path::new("/project");
        let ls = Tool::Bash { command: "ls" };
        let space = Tool::Bash { command: " ls" };

        for rule in [
            "Bash(:*)",     // empty prefix
            " Bash(ls:*)",  // space before the name
            "bash(ls:*)",   // names are case-sensitive
            "Shell(ls:*)",  // unknown tool
            "Bash(ls:*",    // no closing bracket
            "Bash)ls:*(",   // brackets in the wrong order
            "Bash(\"ls\")", // quotes are part of the pattern
            "Bash()",       // matches only an empty command
        ] {
            let c = config(&[rule], &[]);
            assert_eq!(c.check(&ls, p), None, "{rule:?} allowed ls");
            assert_eq!(c.check(&space, p), None, "{rule:?} allowed \" ls\"");
        }

        // The wildcard alone allows every command, chained ones too.
        let c = config(&["Bash(*)"], &[]);
        assert_eq!(c.check(&Tool::Bash { command: "ls; rm x" }, p), Some(true));
        // Spaces inside the pattern are part of it.
        let c = config(&["Bash(git status:*)"], &[]);
        assert_eq!(c.check(&Tool::Bash { command: "git status -s" }, p), Some(true));
        assert_eq!(c.check(&Tool::Bash { command: "git statusx" }, p), None);
    }

    #[test]
    fn test_config_bash_rules() {
        let config = PermissionConfig {
            allow: vec!["Bash(psql:*)".to_string(), "Bash(find:*)".to_string()],
            ..Default::default()
        };

        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Bash {
                    command: "psql -U admin"
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Bash {
                    command: "find . -name '*.rs'"
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Bash {
                    command: "rm -rf /"
                },
                project
            ),
            None
        );
    }

    #[test]
    fn test_config_file_in_project_dir() {
        let config = PermissionConfig::default();
        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Read {
                    path: Path::new("/project/src/main.rs")
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Read {
                    path: Path::new("/other/secret.txt")
                },
                project
            ),
            None
        );
    }

    #[test]
    fn test_config_additional_directories() {
        let config = PermissionConfig {
            additional_directories: vec![PathBuf::from("/extra/allowed")],
            ..Default::default()
        };

        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Write {
                    path: Path::new("/extra/allowed/file.txt")
                },
                project
            ),
            Some(true)
        );
    }

    #[test]
    fn test_deny_overrides_allow() {
        let config = PermissionConfig {
            allow: vec!["Bash(*)".to_string()],
            deny: vec!["Bash(rm:*)".to_string()],
            ..Default::default()
        };

        let project = Path::new("/project");

        assert_eq!(
            config.check(&Tool::Bash { command: "ls" }, project),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Bash {
                    command: "rm -rf /"
                },
                project
            ),
            Some(false)
        );
    }

    #[test]
    fn test_glob_grep_always_allowed() {
        let config = PermissionConfig::default();
        let project = Path::new("/project");

        assert_eq!(config.check(&Tool::Glob, project), Some(true));
        assert_eq!(config.check(&Tool::Grep, project), Some(true));
    }

    #[test]
    fn test_git_readonly_allowed() {
        let config = PermissionConfig::default();
        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Git {
                    subcommand: "status"
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(&Tool::Git { subcommand: "log" }, project),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Git {
                    subcommand: "commit"
                },
                project
            ),
            None
        );
        assert_eq!(
            config.check(&Tool::Git { subcommand: "push" }, project),
            None
        );
    }

    #[test]
    fn test_git_write_allowed_with_rule() {
        let config = PermissionConfig {
            allow: vec!["Git(commit:*)".to_string(), "Git(push:*)".to_string()],
            ..Default::default()
        };
        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Git {
                    subcommand: "commit"
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(&Tool::Git { subcommand: "push" }, project),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Git {
                    subcommand: "reset"
                },
                project
            ),
            None
        );
    }

    #[test]
    fn test_edit_in_project_dir() {
        let config = PermissionConfig::default();
        let project = Path::new("/project");

        assert_eq!(
            config.check(
                &Tool::Edit {
                    path: Path::new("/project/src/lib.rs")
                },
                project
            ),
            Some(true)
        );
        assert_eq!(
            config.check(
                &Tool::Edit {
                    path: Path::new("/other/file.rs")
                },
                project
            ),
            None
        );
    }

    #[test]
    fn sandbox_changes_always_prompt_unless_denied() {
        let config = PermissionConfig {
            allow: vec!["Write(*)".to_string()],
            deny: vec!["Write(.env)".to_string()],
            additional_directories: Vec::new(),
        };
        let project = Path::new("/p");

        let safe = [FileChange::Written("src/main.rs".into(), Vec::new())];
        let secret = [
            FileChange::Written("src/main.rs".into(), Vec::new()),
            FileChange::Deleted(".env".into()),
        ];

        let apply = |changes| Tool::ApplyChanges {
            command: "make",
            changes,
        };

        // Even with Write(*) allowed, the user sees the set.
        assert_eq!(config.check(&apply(&safe), project), None);
        assert_eq!(config.check(&apply(&secret), project), Some(false));
    }
}
