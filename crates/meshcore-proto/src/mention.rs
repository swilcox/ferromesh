//! Naming someone in a message: `@[their name]`.
//!
//! MeshCore has no protocol field for a mention. It's plain text inside the
//! message body, like the `Sender: ` prefix a channel message already
//! carries, and the brackets are there to delimit a name with spaces in it:
//! `@[Chicken Little]`. People type and read `@name`; only the wire carries
//! the brackets, so [`runs`] strips them again for display.
//!
//! This isn't in MeshCore's own documentation — the app is closed and the
//! request for replies (meshcore-dev/MeshCore#618) was closed without a
//! stated convention. It is what other clients have settled on, which is
//! what matters for them to recognise ours: MeshMonitor writes `@[Name]`
//! and calls it "MeshCore's mention form", meshcadet documents the same
//! wire convention, and MeshCoreOne matches `@\[([^\]]+)\]`.

/// `@[name]`, as it travels. Nothing follows it: some clients prefill a
/// reply with a trailing `:`, but most mentions in the wild carry none, so
/// callers add their own separator. A name containing `]` can't be
/// delimited, so it's left as plain text rather than written as a mention
/// nobody can parse.
pub fn wrap(name: &str) -> String {
    if name.contains(']') || name.is_empty() {
        return name.to_owned();
    }
    format!("@[{name}]")
}

/// A stretch of a message: plain text, or a name someone was mentioned by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run<'a> {
    Text(&'a str),
    /// The name inside the brackets, which is what to show after an `@`.
    Mention(&'a str),
}

/// Splits a message into plain text and mentions, in order. A mention only
/// starts the text or follows a space, so `a@[b` is an address and not a
/// mention, and an unclosed `@[` is text like any other: nothing is ever
/// swallowed.
pub fn runs(text: &str) -> Vec<Run<'_>> {
    let mut runs = Vec::new();
    let mut rest = text;
    while let Some(start) = find_mention(rest) {
        let Some(len) = rest[start + 2..].find(']') else { break };
        let (before, mention) = (&rest[..start], &rest[start + 2..start + 2 + len]);
        if !before.is_empty() {
            runs.push(Run::Text(before));
        }
        runs.push(Run::Mention(mention));
        rest = &rest[start + 2 + len + 1..];
    }
    if !rest.is_empty() {
        runs.push(Run::Text(rest));
    }
    runs
}

/// Where the next mention starts, if one does.
fn find_mention(text: &str) -> Option<usize> {
    let mut at = 0;
    while let Some(found) = text[at..].find("@[") {
        let start = at + found;
        let after_space =
            text[..start].chars().next_back().is_none_or(|before| before.is_whitespace());
        if after_space {
            return Some(start);
        }
        at = start + 2;
    }
    None
}

/// Whether `name` is mentioned, ignoring case as names are displayed.
pub fn mentions(text: &str, name: &str) -> bool {
    runs(text)
        .into_iter()
        .any(|run| matches!(run, Run::Mention(mentioned) if mentioned.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping() {
        assert_eq!(wrap("KK4SW"), "@[KK4SW]");
        assert_eq!(wrap("Chicken Little"), "@[Chicken Little]");
        // Nothing sensible to write, so it stays plain.
        assert_eq!(wrap("odd]name"), "odd]name");
        assert_eq!(wrap(""), "");
    }

    #[test]
    fn splitting() {
        assert_eq!(runs(""), []);
        assert_eq!(runs("plain words"), [Run::Text("plain words")]);
        assert_eq!(
            runs("@[KK4SW]: are you there?"),
            [Run::Mention("KK4SW"), Run::Text(": are you there?")]
        );
        assert_eq!(
            runs("ask @[Chicken Little] or @[Bob] later"),
            [
                Run::Text("ask "),
                Run::Mention("Chicken Little"),
                Run::Text(" or "),
                Run::Mention("Bob"),
                Run::Text(" later"),
            ]
        );
        // A mention starts the text or follows a space, so an address isn't
        // one, and an unclosed bracket is just text.
        assert_eq!(
            runs("email a@[b and @[Bob]"),
            [Run::Text("email a@[b and "), Run::Mention("Bob")]
        );
        assert_eq!(runs("half @[open"), [Run::Text("half @[open")]);
        assert_eq!(runs("(@[Bob])"), [Run::Text("(@[Bob])")], "not after a bracket either");
    }

    #[test]
    fn finding_yourself() {
        assert!(mentions("@[KK4SW]: hello", "KK4SW"));
        assert!(mentions("hello @[kk4sw]", "KK4SW"));
        assert!(!mentions("hello KK4SW", "KK4SW"), "only a real mention counts");
        assert!(!mentions("@[KK4SWX] hi", "KK4SW"));
    }
}
