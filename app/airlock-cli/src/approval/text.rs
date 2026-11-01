//! Safe text for the approval prompt.
//!
//! The agent in the sandbox writes the command, the hosts, the rule names
//! and the rule contents. This text is not trusted. It must not change the look of the prompt,
//! hide parts of it or make it too long.

use super::{GrantPrompt, PromptCommand, PromptRequest};
use crate::config::config_values::NetworkRule;
use crate::messages::grants as msg;

/// Maximum length of a rule description, in characters.
pub(crate) const MAX_DESCRIPTION: usize = 200;
/// Maximum length of a rule name, in characters.
const MAX_RULE_NAME: usize = 64;
/// Maximum length of one host pattern, in characters.
const MAX_HOST: usize = 100;
/// Maximum length of the sandbox name, in characters.
const MAX_SANDBOX: usize = 60;
/// Maximum number of hosts in the notification body.
const MAX_BODY_HOSTS: usize = 5;
/// Maximum number of entries of one list in the details.
const MAX_LIST: usize = 64;
/// Maximum length of the command in the details, in characters.
const MAX_COMMAND: usize = 4000;
/// Maximum length of the working directory, in characters.
const MAX_PATH: usize = 300;
/// Maximum number of lines of a diff in the details.
const MAX_DIFF_LINES: usize = 300;
/// Maximum length of one environment value, in characters.
const MAX_ENV_VALUE: usize = 300;

/// Maximum number of characters that one escape sequence can hide. A
/// longer sequence is not complete: only its ESC character is removed.
const MAX_ESCAPE: usize = 64;

/// Number of characters at the start of a cut host pattern. The rest of
/// the limit shows the end, because the end is the significant part.
const HOST_HEAD: usize = 20;

/// Clean untrusted text for one line of the prompt.
/// Removes terminal escape sequences, control characters and invisible
/// format characters, and collapses whitespace.
pub(crate) fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        i += c.len_utf8();
        if c == '\x1b' {
            i += escape_len(&text[i..]);
            space = true;
            continue;
        }
        if c.is_whitespace() || c.is_control() || is_invisible(c) {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

/// Get the length in bytes of the escape sequence after an ESC character.
/// Knows CSI (`ESC [ ... final`), string sequences such as OSC
/// (`ESC ] ... BEL` or `ESC ] ... ESC \`), the known two-character
/// sequences and character set selection.
///
/// Returns:
///   Length to skip. Zero when the sequence is not complete in
///   `MAX_ESCAPE` characters, so that the text after it stays visible.
fn escape_len(rest: &str) -> usize {
    let mut it = rest.char_indices();
    match it.next() {
        Some((_, '[')) => {
            for (i, c) in it.take(MAX_ESCAPE) {
                if ('@'..='~').contains(&c) {
                    return i + c.len_utf8();
                }
                // Only parameter and intermediate bytes come before the
                // final byte.
                if !(' '..='?').contains(&c) {
                    break;
                }
            }
            0
        }
        Some((_, ']' | 'P' | 'X' | '_' | '^')) => {
            let mut esc = false;
            for (i, c) in it.take(MAX_ESCAPE) {
                if c == '\x07' || (esc && c == '\\') {
                    return i + c.len_utf8();
                }
                esc = c == '\x1b';
            }
            0
        }
        // Two-character sequences that terminals know.
        Some((_, c @ ('c' | '7' | '8' | '=' | '>' | '\\'))) => c.len_utf8(),
        // Character set selection: one more character follows.
        Some((_, '(' | ')' | '*' | '+')) => match it.next() {
            Some((i, f)) if ('0'..='~').contains(&f) => i + f.len_utf8(),
            _ => 0,
        },
        _ => 0,
    }
}

/// Tell if `c` is a format character that is not visible or that changes
/// the text direction. Such characters can hide or reorder text.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// Cut `text` at the end to `max` characters. The last kept character is
/// `…` when the text is cut.
pub(crate) fn cut_end(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Cut `text` in the middle to `max` characters, so that its start and
/// its end stay visible. A `…` replaces the removed part.
/// Args:
///  - `text`: Text to cut
///  - `max`: Maximum length in characters
///  - `head`: Number of characters to keep at the start. The rest of the
///    limit keeps the end.
fn cut_middle(text: String, max: usize, head: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text;
    }
    let head = head.min(max.saturating_sub(1));
    let tail = max.saturating_sub(head + 1);
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(len - tail));
    out
}

/// Short host list for the notification body: at most
/// `MAX_BODY_HOSTS` hosts, then the count of the others. `*` shows as all
/// hosts.
fn host_summary(hosts: &[String]) -> String {
    let mut out = hosts
        .iter()
        .take(MAX_BODY_HOSTS)
        .map(|h| show_host(h))
        .collect::<Vec<_>>()
        .join(", ");
    if hosts.len() > MAX_BODY_HOSTS {
        out.push(' ');
        out.push_str(&msg::more_hosts_short(hosts.len() - MAX_BODY_HOSTS));
    }
    out
}

/// Clean host list for the details: one host on each line. Shows at most
/// `MAX_LIST` hosts.
fn host_lines(hosts: &[String]) -> String {
    let mut shown: Vec<String> = hosts.iter().take(MAX_LIST).map(|h| show_host(h)).collect();
    if hosts.len() > MAX_LIST {
        shown.push(msg::more_hosts(hosts.len() - MAX_LIST));
    }
    shown.join("\n")
}

/// Clean one host pattern for the prompt. `*` shows as all hosts.
fn show_host(host: &str) -> String {
    if host.trim() == "*" {
        msg::ALL_HOSTS.to_string()
    } else {
        clean_host(host)
    }
}

/// Clean one host pattern and cut it in the middle, so that its end stays
/// visible.
fn clean_host(host: &str) -> String {
    cut_middle(clean(host), MAX_HOST, HOST_HEAD)
}

/// Make the TOML text of a rule, with clean values. Each list shows at
/// most `MAX_LIST` entries.
fn rule_toml(name: &str, rule: &NetworkRule) -> String {
    let shown = |list: &[String]| -> Vec<String> {
        list.iter().take(MAX_LIST).map(|h| clean_host(h)).collect()
    };
    let clean_rule = NetworkRule {
        enabled: rule.enabled,
        description: cut_end(clean(&rule.description), MAX_DESCRIPTION),
        allow: shown(&rule.allow),
        deny: shown(&rule.deny),
        passthrough: rule.passthrough,
        inject: shown(&rule.inject),
    };
    let mut text =
        crate::network::rule_file::rule_toml(&cut_end(clean(name), MAX_RULE_NAME), &clean_rule);
    for (key, list) in [
        ("allow", &rule.allow),
        ("deny", &rule.deny),
        ("inject", &rule.inject),
    ] {
        if list.len() > MAX_LIST {
            text.push_str(&msg::more_patterns(key, list.len() - MAX_LIST));
            text.push('\n');
        }
    }
    text.trim_end().to_string()
}

/// Make the lines of the other keys of a rule table, with clean keys and
/// values. Each line starts with a new line. At most `MAX_LIST` keys
/// show.
fn other_lines(other: &[(String, String)]) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    for (key, value) in other.iter().take(MAX_LIST) {
        let key = cut_end(clean(key), MAX_RULE_NAME);
        let value = cut_end(clean(value), MAX_DESCRIPTION);
        let _ = write!(text, "\n{key} = {value}");
    }
    if other.len() > MAX_LIST {
        text.push('\n');
        text.push_str(&msg::more_patterns("keys", other.len() - MAX_LIST));
    }
    text
}

/// Clean untrusted text that can have more lines. Each line is cleaned as
/// in [`clean`]. Empty lines go away. The text is cut at `max` characters.
fn clean_lines(text: &str, max: usize) -> String {
    let lines: Vec<String> = text.lines().map(clean).filter(|l| !l.is_empty()).collect();
    cut_end(lines.join("\n"), max)
}

/// Clean environment entries for the details: one `KEY='VALUE'` entry on
/// each line, with shell quotes. Shows at most `MAX_LIST` entries.
fn env_lines(env: &[String]) -> String {
    let mut shown: Vec<String> = env
        .iter()
        .take(MAX_LIST)
        .map(|entry| {
            let (key, val) = entry.split_once('=').unwrap_or((entry, ""));
            let key = cut_end(clean(key), MAX_RULE_NAME);
            let val = cut_end(clean(val), MAX_ENV_VALUE).replace('\'', "'\\''");
            format!("{key}='{val}'")
        })
        .collect();
    if env.len() > MAX_LIST {
        shown.push(msg::more_entries(env.len() - MAX_LIST));
    }
    shown.join("\n")
}

/// One labeled value in the details of a request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct DetailItem {
    /// Label of the value.
    pub label: String,
    /// Clean value. It can have more lines.
    pub value: String,
    /// True if the value shows in a monospaced font.
    #[serde(default)]
    pub mono: bool,
    /// Lines of a change, if the value is a diff. `value` then has the
    /// same lines as plain text with `+ `, `- ` and `  ` marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Vec<DiffLine>>,
}

impl DetailItem {
    fn new(label: &str, value: String, mono: bool) -> Self {
        Self {
            label: label.to_string(),
            value,
            mono,
            diff: None,
        }
    }

    /// Make a diff value from clean lines.
    fn diff(label: &str, lines: Vec<DiffLine>) -> Self {
        let value = lines
            .iter()
            .map(|l| format!("{}{}", l.kind.mark(), l.text))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            label: label.to_string(),
            value,
            mono: true,
            diff: Some(lines),
        }
    }
}

/// Kind of one diff line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum DiffKind {
    /// The line is new.
    Add,
    /// The line goes away.
    Remove,
    /// The line does not change.
    Same,
}

impl DiffKind {
    /// Mark of the line in the plain text form.
    fn mark(self) -> &'static str {
        match self {
            DiffKind::Add => "+ ",
            DiffKind::Remove => "- ",
            DiffKind::Same => "  ",
        }
    }
}

/// One line of a diff.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct DiffLine {
    /// Kind of the line.
    pub kind: DiffKind,
    /// Clean text of the line, with no mark.
    pub text: String,
}

/// Make the line diff from `old` to `new`. Shows at most `MAX_DIFF_LINES`
/// lines.
fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut lines: Vec<DiffLine> = diff
        .iter_all_changes()
        .map(|change| DiffLine {
            kind: match change.tag() {
                similar::ChangeTag::Insert => DiffKind::Add,
                similar::ChangeTag::Delete => DiffKind::Remove,
                similar::ChangeTag::Equal => DiffKind::Same,
            },
            text: change.value().trim_end_matches('\n').to_string(),
        })
        .collect();
    if lines.len() > MAX_DIFF_LINES {
        let more = lines.len() - MAX_DIFF_LINES;
        lines.truncate(MAX_DIFF_LINES);
        lines.push(DiffLine {
            kind: DiffKind::Same,
            text: msg::more_lines(more),
        });
    }
    lines
}

/// Make the details of a command request.
fn command_items(command: &PromptCommand) -> Vec<DetailItem> {
    let mut items = vec![
        DetailItem::new(
            msg::LABEL_COMMAND,
            clean_lines(&command.cmd, MAX_COMMAND),
            true,
        ),
        DetailItem::new(
            msg::LABEL_CWD,
            cut_middle(clean(&command.cwd), MAX_PATH, HOST_HEAD),
            true,
        ),
    ];
    if !command.env.is_empty() {
        items.push(DetailItem::new(
            msg::LABEL_ENV,
            env_lines(&command.env),
            true,
        ));
    }
    items
}

/// Prompt text in a safe form, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PromptText {
    /// Title line.
    pub title: String,
    /// Short body.
    pub body: String,
    /// Clean sandbox name.
    pub sandbox: String,
    /// Body of the notice while the approved command runs. `None` if the
    /// request has no such notice.
    pub running: Option<String>,
    /// All details of the request, for the details window.
    pub details: Vec<DetailItem>,
}

impl PromptText {
    /// Make the safe prompt text from a request.
    pub(super) fn new(prompt: &GrantPrompt) -> Self {
        let sandbox = cut_end(clean(&prompt.sandbox), MAX_SANDBOX);
        let mut details = vec![DetailItem::new(msg::LABEL_SANDBOX, sandbox.clone(), false)];
        let clean_name = |name: &str| cut_end(clean(name), MAX_RULE_NAME);
        let (title, body, command, running) = match &prompt.request {
            PromptRequest::Hosts { hosts, command } => {
                details.extend(command_items(command));
                details.push(DetailItem::new(msg::LABEL_HOSTS, host_lines(hosts), true));
                let body = host_summary(hosts);
                let running = format!("{}\n{body}", msg::RUNNING);
                (
                    msg::ACCESS_TITLE.to_string(),
                    body,
                    Some(command),
                    Some(running),
                )
            }
            PromptRequest::Rule {
                name,
                rule,
                command,
            } => {
                details.extend(command_items(command));
                details.push(DetailItem::new(
                    msg::LABEL_RULE,
                    rule_toml(name, rule),
                    true,
                ));
                let running = format!("{}\n{}", msg::RUNNING, msg::rule_line(clean_name(name)));
                (
                    msg::access_rule_title(clean_name(name)),
                    msg::CLICK_FOR_DETAILS.to_string(),
                    Some(command),
                    Some(running),
                )
            }
            PromptRequest::RuleChange {
                name,
                old,
                rule,
                other,
            } => {
                // The full table: all keys of the rule, then the other keys
                // of the table in the file.
                let other = other_lines(other);
                let old = old
                    .as_ref()
                    .map(|o| rule_toml(name, o) + &other)
                    .unwrap_or_default();
                let new = rule_toml(name, rule) + &other;
                details.push(DetailItem::diff(msg::LABEL_CHANGES, diff_lines(&old, &new)));
                (
                    msg::rule_change_title(clean_name(name)),
                    msg::CLICK_FOR_CHANGES.to_string(),
                    None,
                    None,
                )
            }
        };
        if let Some(command) = command {
            details.push(DetailItem::new(
                msg::LABEL_DURATION,
                msg::duration(command.duration_secs),
                false,
            ));
        }
        Self {
            title,
            body,
            sandbox,
            running,
            details,
        }
    }

    /// Get the details as plain text: each label on its own line, then the
    /// value.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn details_text(&self) -> String {
        self.details
            .iter()
            .map(|item| format!("{}:\n{}", item.label, item.value))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Escape the characters that have a meaning in notification body markup.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(super) fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that terminal escape sequences, control characters and
    /// invisible characters do not get into the prompt.
    ///   1. Clean a text with CSI, OSC, BEL, newlines and bidi overrides
    ///   2. Check that only the visible words stay, with single spaces
    #[test]
    fn cleaning_text_with_escapes_and_controls_keeps_only_visible_words() {
        let raw = "\x1b[31mred\x1b[0m\x1b]0;title\x07 ok\r\n\tnext\u{202e}evil\u{200b}x\x07 \x1b]8;;http://a\x1b\\link\u{e0041}\u{fe0f}\u{034f}";
        assert_eq!(clean(raw), "red ok next evil x link");
    }

    /// Test that an escape sequence without an end does not hide the text
    /// after it. Otherwise an agent can hide a part of the command.
    ///   1. Clean texts with an OSC and a CSI that do not end
    ///   2. Check that only the ESC is removed
    #[test]
    fn cleaning_unterminated_escape_keeps_rest_visible() {
        assert_eq!(clean("ls \x1b] ; curl evil"), "ls ] ; curl evil");
        assert_eq!(clean("ls \x1b[1\n curl evil"), "ls [1 curl evil");
        assert_eq!(clean("go \x1bevil.com"), "go evil.com");
        assert_eq!(clean("a\x1b(Bb\x1b7c"), "a b c");
        let long = format!("a\x1b]{}\x07b", "x".repeat(100));
        assert!(clean(&long).contains("xxx"));
    }

    /// Test that text over the limit is cut at the end with a mark, and
    /// that text in the limit does not change.
    ///   1. Cut a text longer than the limit
    ///   2. Check the length and the end mark
    ///   3. Check that multi-byte characters are counted as characters
    #[test]
    fn cutting_long_text_at_end_keeps_limit_with_mark() {
        let out = cut_end("a".repeat(500), 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('…'));
        assert_eq!(cut_end("ääää".into(), 4), "ääää");
        assert_eq!(cut_end("äääää".into(), 4), "äää…");
    }

    /// Test that a long host pattern keeps its end. The end of a host name
    /// tells its owner, so it must always show.
    ///   1. Format a host list with one very long host name
    ///   2. Check the length, the start and the full end of the name
    #[test]
    fn cutting_long_host_keeps_its_end() {
        let host = format!("{}.bbb.evil.com", "a".repeat(200));
        let out = host_lines(&[host]);
        assert_eq!(out.chars().count(), MAX_HOST);
        assert!(out.starts_with(&"a".repeat(HOST_HEAD)));
        assert!(out.ends_with("aaa.bbb.evil.com"));
        assert_eq!(out.matches('…').count(), 1);
    }

    /// Test the short notification text of each request kind. The body
    /// shows at most five hosts and a count of the others, and `*` shows as
    /// all hosts. A rule request and a rule change show the rule name.
    ///   1. Make host requests with 7 hosts and with `*`, and check the
    ///      bodies
    ///   2. Make a rule request and check the body and the running notice
    ///   3. Make a rule change and check the title and the body
    #[test]
    fn prompt_body_is_short_for_hosts_rule_and_rule_change() {
        let prompt = |request| GrantPrompt {
            sandbox: "my-proj".into(),
            request,
        };
        let hosts = |hosts: Vec<String>| PromptRequest::Hosts {
            hosts,
            command: command(),
        };
        let list: Vec<String> = (0..7).map(|i| format!("h{i}.example.com")).collect();
        let text = PromptText::new(&prompt(hosts(list)));
        assert_eq!(text.title, "Network access requested");
        assert_eq!(
            text.body,
            "h0.example.com, h1.example.com, h2.example.com, h3.example.com, \
             h4.example.com + 2 more"
        );
        assert_eq!(text.sandbox, "my-proj");
        let text = PromptText::new(&prompt(hosts(vec!["*".into()])));
        assert_eq!(text.body, "all hosts");

        let text = PromptText::new(&prompt(PromptRequest::Rule {
            name: "npm".into(),
            rule: rule(),
            command: command(),
        }));
        assert_eq!(text.title, "Network access requested (npm)");
        assert_eq!(text.body, "Click for more details");
        assert_eq!(text.running.as_deref(), Some("Running...\nRule: npm"));

        let text = PromptText::new(&prompt(PromptRequest::RuleChange {
            name: "npm".into(),
            old: None,
            rule: rule(),
            other: vec![],
        }));
        assert_eq!(text.title, "Network rule change requested: npm");
        assert_eq!(text.body, "Click to see changes");
        assert_eq!(text.running, None);
    }

    /// Test the diff of a rule change. A new rule shows only added lines.
    /// A changed rule shows the removed, the added and the same lines, so
    /// that the user sees what changes. The diff shows the full rule table:
    /// `passthrough`, `inject` and the other keys of the table in the file
    /// also show when they do not change.
    ///   1. Make a rule change with no old rule and check that all lines
    ///      are added
    ///   2. Make a change of the allow list of a rule with one other key,
    ///      and check each diff line and the plain text form
    #[test]
    fn rule_change_details_show_diff_of_full_rule() {
        let change = |old: Option<NetworkRule>| {
            let other = if old.is_some() {
                vec![("future_key".to_string(), "\"x\"".to_string())]
            } else {
                vec![]
            };
            let mut new = rule();
            new.allow.push("npm.example.com".into());
            PromptText::new(&GrantPrompt {
                sandbox: "my-proj".into(),
                request: PromptRequest::RuleChange {
                    name: "npm".into(),
                    old,
                    rule: new,
                    other,
                },
            })
            .details
            .pop()
            .unwrap()
        };
        let item = change(None);
        assert_eq!(item.label, "Changes");
        let diff = item.diff.unwrap();
        assert_eq!(diff.len(), 7);
        assert!(diff.iter().all(|l| l.kind == DiffKind::Add));

        let item = change(Some(rule()));
        let lines: Vec<(DiffKind, &str)> = item
            .diff
            .as_ref()
            .unwrap()
            .iter()
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (DiffKind::Same, "[network.rules.npm]"),
                (DiffKind::Same, "enabled = \"on-demand\""),
                (DiffKind::Same, "description = 'npm [network] \"x\"'"),
                (DiffKind::Remove, "allow = [\"registry.npmjs.org\"]"),
                (
                    DiffKind::Add,
                    "allow = [\"registry.npmjs.org\", \"npm.example.com\"]"
                ),
                (DiffKind::Same, "deny = []"),
                (DiffKind::Same, "passthrough = false"),
                (DiffKind::Same, "inject = [\"NPM_TOKEN\"]"),
                (DiffKind::Same, "future_key = \"x\""),
            ]
        );
        assert!(
            item.value
                .starts_with("  [network.rules.npm]\n  enabled = \"on-demand\"\n"),
            "{}",
            item.value
        );
        assert!(
            item.value
                .contains("\n- allow = [\"registry.npmjs.org\"]\n+ allow")
        );
    }

    /// Test the details of a command with a rule. The details show the
    /// clean command, the env with shell quotes, and the full rule as clean
    /// TOML with its `inject` and `passthrough` keys. The user must see
    /// which secrets the command can use: `inject` shows the names only.
    ///   1. Make a rule request with agent text that has escapes, quotes
    ///      and new lines
    ///   2. Check each label and value
    #[test]
    fn details_show_command_env_and_rule_toml() {
        let text = PromptText::new(&GrantPrompt {
            sandbox: "my-proj".into(),
            request: PromptRequest::Rule {
                name: "npm".into(),
                rule: rule(),
                command: command(),
            },
        });
        let items: Vec<(&str, &str, bool)> = text
            .details
            .iter()
            .map(|i| (i.label.as_str(), i.value.as_str(), i.mono))
            .collect();
        assert_eq!(
            items,
            [
                ("Sandbox", "my-proj", false),
                ("Command", "npm install\nnpm test", true),
                ("Working directory", "/work", true),
                ("Extra environment", "A='it'\\''s'\nB=''", true),
                (
                    "Rule",
                    "[network.rules.npm]\n\
                     enabled = \"on-demand\"\n\
                     description = 'npm [network] \"x\"'\n\
                     allow = [\"registry.npmjs.org\"]\n\
                     deny = []\n\
                     passthrough = false\n\
                     inject = [\"NPM_TOKEN\"]",
                    true
                ),
                ("Maximum duration", "1 h", false),
            ]
        );
    }

    fn command() -> PromptCommand {
        PromptCommand {
            cmd: "npm install\x1b[2K\n\nnpm test".into(),
            cwd: "/work".into(),
            env: vec!["A=it's".into(), "B=".into()],
            duration_secs: 3600,
        }
    }

    fn rule() -> NetworkRule {
        NetworkRule {
            enabled: crate::config::config_values::RuleEnabled::OnDemand,
            description: "npm\n[network] \"x\"".into(),
            allow: vec!["registry.npmjs.org".into()],
            deny: vec![],
            passthrough: false,
            inject: vec!["NPM_TOKEN".into()],
        }
    }

    /// Test that markup characters are escaped so that the notification
    /// server cannot show agent text as links or formatting.
    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(
            escape_markup("<a href='x'>&</a>"),
            "&lt;a href=&apos;x&apos;&gt;&amp;&lt;/a&gt;"
        );
    }
}
