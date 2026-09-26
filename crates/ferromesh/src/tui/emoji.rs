//! `:shortcode:` emoji in the compose line: the partial code being typed,
//! the emoji it might mean, and finishing it off. Shortcodes are GitHub's,
//! as Slack and Discord mostly use too.

/// How many suggestions to offer at once.
pub const SHOWN: usize = 8;

/// Typing this much of a code, after the colon, brings up suggestions; any
/// less and `:p` or `:3` would pop them up mid-sentence.
const MIN_QUERY: usize = 2;

/// An emoji offered for the code being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suggestion {
    pub emoji: &'static str,
    pub shortcode: &'static str,
}

/// The code being typed at the end of `text`: the byte offset of its colon,
/// and what follows it. The colon has to start a word, so a time like
/// `12:30` isn't taken for one.
pub fn pending(text: &str) -> Option<(usize, &str)> {
    let colon = text.rfind(':')?;
    let query = &text[colon + 1..];
    let starts_word = text[..colon].chars().next_back().is_none_or(char::is_whitespace);
    (starts_word && query.len() >= MIN_QUERY && query.chars().all(is_code_char))
        .then_some((colon, query))
}

/// The emoji whose shortcodes match `query`, best first: an exact match,
/// then codes that start with it, then codes with a word that does, then
/// codes that merely contain it; shorter codes first within each.
pub fn suggest(query: &str) -> Vec<Suggestion> {
    let query = query.to_ascii_lowercase();
    let mut found: Vec<(u8, Suggestion)> = emojis::iter()
        .filter_map(|emoji| {
            emoji
                .shortcodes()
                .filter_map(|code| rank(code, &query).map(|rank| (rank, code)))
                .min_by_key(|&(rank, code)| (rank, code.len()))
                .map(|(rank, shortcode)| (rank, Suggestion { emoji: emoji.as_str(), shortcode }))
        })
        .collect();
    found.sort_by_key(|(rank, suggestion)| (*rank, suggestion.shortcode.len()));
    found.into_iter().take(SHOWN).map(|(_, suggestion)| suggestion).collect()
}

/// `text` with the code being typed swapped for `emoji`.
pub fn accept(text: &str, emoji: &str) -> Option<String> {
    let (colon, _) = pending(text)?;
    Some(format!("{}{emoji}", &text[..colon]))
}

/// When `text` ends in a whole `:shortcode:` just closed, the text with the
/// emoji in its place.
pub fn close(text: &str) -> Option<String> {
    let open = text.strip_suffix(':')?;
    let (colon, code) = pending(open)?;
    let emoji = emojis::get_by_shortcode(&code.to_ascii_lowercase())?;
    Some(format!("{}{}", &open[..colon], emoji.as_str()))
}

fn is_code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')
}

fn rank(code: &str, query: &str) -> Option<u8> {
    if code == query {
        Some(0)
    } else if code.starts_with(query) {
        Some(1)
    } else if code.split('_').skip(1).any(|word| word.starts_with(query)) {
        Some(2)
    } else if code.contains(query) {
        Some(3)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_starts_a_word_and_runs_to_the_end() {
        assert_eq!(pending("hi :smi"), Some((3, "smi")));
        assert_eq!(pending(":+1"), Some((0, "+1")));
        assert_eq!(pending("hi :s"), None, "too short yet");
        assert_eq!(pending("at 12:30"), None, "a time, not a code");
        assert_eq!(pending("hi :smile there"), None, "already past it");
        assert_eq!(pending("hi :smile:"), None);
    }

    #[test]
    fn exact_and_prefix_matches_come_first() {
        let codes: Vec<_> = suggest("smile").iter().map(|s| s.shortcode).collect();
        assert_eq!(codes[0], "smile");
        assert!(codes.contains(&"smiley"));
        assert!(codes.len() <= SHOWN);
        assert_eq!(suggest("Tada")[0].emoji, "🎉");
        assert!(suggest("zzqq").is_empty());
    }

    #[test]
    fn each_emoji_is_offered_once() {
        let emoji: Vec<_> = suggest("thumbs").iter().map(|s| s.emoji).collect();
        assert_eq!(emoji.iter().filter(|&&e| e == "👍").count(), 1);
    }

    #[test]
    fn accepting_and_closing_replace_the_code() {
        assert_eq!(accept("nice :tad", "🎉").as_deref(), Some("nice 🎉"));
        assert_eq!(close("nice :tada:").as_deref(), Some("nice 🎉"));
        assert_eq!(close("nice :nosuchcode:"), None);
        assert_eq!(close("at 12:30:"), None);
    }
}
