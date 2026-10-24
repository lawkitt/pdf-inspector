//! Line classification: captions, lists, code detection.

/// Check if text is a figure/table caption or source citation
pub(crate) fn is_caption_line(text: &str) -> bool {
    let trimmed = text.trim();

    // Caption prefixes that always match (always followed by identifiers)
    let always_prefixes = [
        "Figura ",
        "Fig. ",
        "Fig ",
        "Tabela ",
        "Source:",
        "Fonte:",
        "Source ",
        "Fonte ",
        "Note:",
        "Nota:",
        "Chart ",
        "Gráfico ",
        "Graph ",
        "Diagram ",
        "Image ",
        "Imagem ",
        "Photo ",
        "Foto ",
    ];
    for prefix in &always_prefixes {
        if trimmed.starts_with(prefix) {
            return true;
        }
    }

    // "Figure" and "Table" need a digit/reference after them to distinguish
    // captions ("Table 1", "Figure 3.2") from headings ("Table of Contents")
    for prefix in ["Figure ", "Table "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest
                .trim_start()
                .starts_with(|c: char| c.is_ascii_digit() || c == '(' || c == '#')
            {
                return true;
            }
        }
    }

    // Check case-insensitive patterns — require digit or punctuation after
    // prefix to avoid matching "Table of Contents" or "Figure drawing" etc.
    let lower = trimmed.to_lowercase();
    for pfx in ["figure ", "table "] {
        if let Some(rest) = lower.strip_prefix(pfx) {
            if rest
                .trim_start()
                .starts_with(|c: char| c.is_ascii_digit() || c == '(' || c == '#')
            {
                return true;
            }
        }
    }
    if lower.starts_with("source:") {
        return true;
    }

    false
}

/// Check if text starts with an unambiguous bullet marker (●, •, ○, ◦).
///
/// Narrower than [`is_list_item`]: it excludes numbered/lettered patterns
/// like `1.` or `a)`, which legitimately appear as section headings in many
/// documents. Used by the heading classifier to reject bullet lines without
/// also demoting numbered headings.
pub(crate) fn starts_with_bullet_marker(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("• ")
        || trimmed.starts_with("● ")
        || trimmed.starts_with("○ ")
        || trimmed.starts_with("◦ ")
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
}

/// Bullet glyphs PDFs paint as list markers; [`format_list_item`] turns
/// them into `-`.
const BULLETS: [char; 6] = ['•', '●', '○', '◦', '▪', '■'];

/// The text after a leading bullet glyph, with or without a space between
/// them ("•Bullet" is a producer that dropped the tab after the marker).
fn after_bullet(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(BULLETS)?.trim_start();
    (!rest.is_empty()).then_some(rest)
}

/// Check if text looks like a list item
pub(crate) fn is_list_item(text: &str) -> bool {
    let trimmed = text.trim_start();

    // Bullet patterns
    if after_bullet(trimmed).is_some() || trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }

    // Numbered list patterns: "1.", "1)", "(1)", "a.", "a)"
    let first_chars: String = trimmed.chars().take(5).collect();
    if first_chars.contains(|c: char| c.is_ascii_digit()) {
        // Check for "1.", "1)", "10."
        if let Some(idx) = first_chars.find(['.', ')']) {
            let prefix = &first_chars[..idx];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }

    // Letter list: "a.", "a)", "(a)"
    let mut chars = trimmed.chars();
    if let (Some(first), Some(second)) = (chars.next(), chars.next()) {
        if first.is_ascii_alphabetic() && (second == '.' || second == ')') {
            return true;
        }
        if first == '(' && chars.next() == Some(')') {
            return true;
        }
    }

    // Roman numerals of more than one letter ("iv.", "XII)") and letters of
    // other scripts ("а)")
    split_list_label(trimmed).is_some()
}

/// Whether `label` is a Roman numeral from 1 to 39 in one case.
fn is_roman(label: &str) -> bool {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static ROMAN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(?:X{0,3}(?:IX|IV|V?I{0,3})|x{0,3}(?:ix|iv|v?i{0,3}))$").unwrap()
    });
    !label.is_empty() && ROMAN_RE.is_match(label)
}

/// What a list label numbers with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Label {
    /// "1." / "1)": a Markdown ordered-list marker.
    Decimal,
    /// "a)", "iv.", "(a)", "(1)": no Markdown equivalent.
    Other,
}

/// Whether `formatted`, a line from [`format_list_item`], opens a Markdown
/// list item. A label that cannot ("1.1 Scope", "(b)." ending a sentence)
/// stays plain text.
pub(crate) fn starts_markdown_list_item(formatted: &str) -> bool {
    let digits = formatted.bytes().take_while(u8::is_ascii_digit).count();
    let after = &formatted[digits..];
    formatted.starts_with("- ")
        || ((1..=9).contains(&digits) && (after.starts_with(". ") || after.starts_with(") ")))
}

/// "* * * * *", "***": a thematic break, not a bullet.
pub(crate) fn is_thematic_break(text: &str) -> bool {
    let marks: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && marks.chars().all(|c| c == '*')
}

/// Split a leading list label from its text: "1.First" -> ("1.", "First"),
/// "a) Alpha" -> ("a)", "Alpha"), "(iv) Text" -> ("(iv)", "Text"). A label
/// is a number, a Roman numeral, one Latin letter, or one letter of any
/// script before ")" ("а)", "б)" in Russian; "т." is an abbreviation). It
/// may run straight into its text (a dropped tab), but not into a digit
/// ("1.5 million") or an abbreviation ("e.g.", "U.S."), and an unspaced
/// capital that is no numeral is an initial ("J.Smith"), as is one before
/// another initial ("J. W. Tukey").
fn split_list_label(text: &str) -> Option<(&str, &str, Label)> {
    let (label_len, body, parenthesized) = match text.strip_prefix('(') {
        Some(inner) => {
            let close = inner.find(')')?;
            (close + 2, &inner[..close], true)
        }
        None => {
            let end = text.find(['.', ')'])?;
            (end + 1, &text[..end], false)
        }
    };
    let mut letters = body.chars();
    let single_letter = match (letters.next(), letters.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => true,
        (Some(c), None) => c.is_alphabetic() && text[label_len - 1..].starts_with(')'),
        _ => false,
    };
    let number = (1..=9).contains(&body.len()) && body.bytes().all(|b| b.is_ascii_digit());
    let kind = if number && !parenthesized {
        Label::Decimal
    } else if number || single_letter || is_roman(body) {
        Label::Other
    } else {
        return None;
    };
    let (label, rest) = text.split_at(label_len);
    let mut after = rest.chars();
    let next = after.next()?;
    if next.is_whitespace() {
        let rest = rest.trim_start();
        // "J. W. Tukey": initials, not a label.
        let mut words = rest.chars();
        let initials = label.ends_with('.')
            && body.bytes().all(|b| b.is_ascii_uppercase())
            && words.next().is_some_and(char::is_uppercase)
            && words.next() == Some('.');
        return (!rest.is_empty() && !initials).then_some((label, rest, kind));
    }
    // Not "(": "(c)(2)" is a cross-reference.
    let runs_into_text = next.is_alphabetic() || matches!(next, '*' | '"' | '“' | '«' | '<');
    let abbreviation = label.ends_with('.') && after.next() == Some('.');
    let initial = !parenthesized
        && body.len() == 1
        && body.bytes().all(|b| b.is_ascii_uppercase())
        && !is_roman(body);
    (runs_into_text && !abbreviation && !initial).then_some((label, rest, kind))
}

/// Format list item to markdown: bullets become `-`, decimal labels stay
/// ordered-list markers ("1. ", "1) "), and labels Markdown cannot number
/// ("a)", "iv.", "(a)") become a `-` item that keeps the label as text, as
/// anydoc renders them. A label run into its text gets its space back.
pub(crate) fn format_list_item(text: &str) -> String {
    let trimmed = text.trim_start();

    // Convert various bullet styles to markdown
    // Note: bullet characters like • are multi-byte in UTF-8, use char indices
    if let Some(rest) = after_bullet(trimmed) {
        return format!("- {rest}");
    }
    // A marker inside a leading style run (e.g. "**● Label:** rest",
    // "<u>● Label</u>", "**a)** rest"). The run wraps both the marker and
    // the following label because both carry the style in the PDF. The
    // marker must move outside the wrapper so markdown still sees a list
    // item; a wrapper left around nothing is dropped.
    for (open, close) in [("**", "**"), ("*", "*"), ("<u>", "</u>")] {
        let Some(inner) = trimmed.strip_prefix(open) else {
            continue;
        };
        let (marker, rest) = if let Some(rest) = after_bullet(inner) {
            ("-".to_string(), rest)
        } else if let Some((label, rest, kind)) = split_list_label(inner) {
            (marker_for(label, kind), rest)
        } else {
            continue;
        };
        return match rest.strip_prefix(close) {
            Some(after) if !after.trim().is_empty() => format!("{marker} {}", after.trim_start()),
            _ => format!("{marker} {open}{rest}"),
        };
    }

    if let Some(rest) = trimmed.strip_prefix("* ") {
        if is_thematic_break(trimmed) {
            return trimmed.to_string();
        }
        return format!("- {}", rest.trim_start());
    }
    if trimmed.starts_with("- ") {
        return trimmed.to_string();
    }

    if let Some((label, rest, kind)) = split_list_label(trimmed) {
        return format!("{} {rest}", marker_for(label, kind));
    }
    trimmed.to_string()
}

/// The Markdown marker for a list label.
fn marker_for(label: &str, kind: Label) -> String {
    match kind {
        Label::Decimal => label.to_string(),
        Label::Other => format!("- {label}"),
    }
}

/// Check if text looks like code
pub(crate) fn is_code_like(text: &str) -> bool {
    let trimmed = text.trim();

    // Code patterns
    let code_patterns = [
        // Language keywords
        "import ",
        "export ",
        "from ",
        "const ",
        "let ",
        "var ",
        "function ",
        "class ",
        "def ",
        "pub fn ",
        "fn ",
        "async fn ",
        "impl ",
        // Syntax patterns
        "=> ",
        "-> ",
        ":: ",
        ":= ",
    ];

    for pattern in &code_patterns {
        if trimmed.starts_with(pattern) {
            return true;
        }
    }

    // Check for code-like syntax
    let special_chars: usize = trimmed
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ';' | '=' | '<' | '>'))
        .count();

    if special_chars >= 3 && trimmed.len() < 200 {
        return true;
    }

    // Ends with semicolon or braces
    if trimmed.ends_with(';') || trimmed.ends_with('{') || trimmed.ends_with('}') {
        return true;
    }

    false
}

/// True when a line's text is essentially all monospace (≥90% by character
/// count). Code lines are wholly monospace; anything less is prose carrying
/// mono-styled fragments — a URL sidebar, or a sentence quoting an inline
/// code literal — and fencing it would split paragraphs mid-sentence.
/// Any-item matching was safe only while items carried opaque font resource
/// names that never matched the monospace patterns; items now carry real
/// family names.
pub(crate) fn line_is_monospace(line: &crate::types::TextLine) -> bool {
    let mut monospace_chars = 0usize;
    let mut total_chars = 0usize;
    for item in &line.items {
        let text = item.text.trim();
        let chars = text.chars().count();
        total_chars += chars;
        // Hyperlinks and underlined text set in a mono face are link
        // styling, not code — a URL sidebar must not fence lyric lines.
        let looks_like_link = item.is_underline
            || matches!(item.item_type, crate::types::ItemType::Link(_))
            || text.contains("://")
            || text.starts_with("www.");
        if is_monospace_font(&item.font) && !looks_like_link {
            monospace_chars += chars;
        }
    }
    total_chars > 0 && monospace_chars * 10 >= total_chars * 9
}

/// Check if font name indicates monospace
pub(crate) fn is_monospace_font(font_name: &str) -> bool {
    let lower = font_name.to_lowercase();
    // "Monotype" is a foundry prefix on proportional faces (Monotype
    // Corsiva, Monotype Garamond) — it must not satisfy the generic "mono"
    // token below.
    if lower.contains("monotype") {
        return false;
    }
    let patterns = [
        "courier",
        "consolas",
        "monaco",
        "menlo",
        "mono",
        "fixed",
        "terminal",
        "typewriter",
        "source code",
        "fira code",
        "jetbrains",
        "inconsolata",
        "dejavu sans mono",
        "liberation mono",
    ];

    patterns.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotype_foundry_faces_are_not_monospace() {
        // "Monotype" is a foundry prefix on proportional faces; the generic
        // "mono" token must not classify them as code fonts.
        assert!(!is_monospace_font("MonotypeCorsiva"));
        assert!(!is_monospace_font("ABCDEF+Monotype-Garamond"));
        assert!(is_monospace_font("RobotoMono-Regular"));
        assert!(is_monospace_font("PTMono"));
        assert!(is_monospace_font("Courier"));
    }

    #[test]
    fn format_list_item_plain_bullet() {
        assert_eq!(format_list_item("● Item"), "- Item");
        assert_eq!(format_list_item("• Item"), "- Item");
    }

    #[test]
    fn format_list_item_bullet_inside_underline() {
        // Fully-underlined bullet line: the marker must move outside the
        // <u> wrapper so markdown still renders a list item.
        assert_eq!(format_list_item("<u>● Item text</u>"), "- <u>Item text</u>");
    }

    #[test]
    fn format_list_item_bullet_inside_bold() {
        // PDF that uses bold font for both the marker and the label produces
        // a single bold run like "**● Label:** rest"; the bullet must still
        // be stripped and the bold wrapper preserved on the label.
        assert_eq!(
            format_list_item("**● Fraud: Willing cooperation;**"),
            "- **Fraud: Willing cooperation;**"
        );
        assert_eq!(
            format_list_item("**● Label:** rest of line"),
            "- **Label:** rest of line"
        );
        assert_eq!(format_list_item("*● Italic:* rest"), "- *Italic:* rest");
    }

    #[test]
    fn format_list_item_already_dash() {
        assert_eq!(format_list_item("- existing"), "- existing");
    }

    #[test]
    fn format_list_item_restores_markers_run_into_text() {
        // A producer that drops the tab after the marker ("1.First").
        assert_eq!(format_list_item("1.First numbered"), "1. First numbered");
        assert_eq!(format_list_item("•Bullet one"), "- Bullet one");
        assert_eq!(format_list_item("◦Nested bullet"), "- Nested bullet");
        assert_eq!(format_list_item("1.Первый пункт"), "1. Первый пункт");
        assert!(is_list_item("•Bullet one"));
        assert!(is_list_item("IV.Roman starting at four"));
    }

    #[test]
    fn format_list_item_keeps_labels_markdown_cannot_number_as_text() {
        assert_eq!(format_list_item("a)Alpha sub one"), "- a) Alpha sub one");
        assert_eq!(format_list_item("i.Roman sub sub"), "- i. Roman sub sub");
        assert_eq!(
            format_list_item("IV.Roman starting at four"),
            "- IV. Roman starting at four"
        );
        assert_eq!(format_list_item("V.Roman five"), "- V. Roman five");
        assert_eq!(
            format_list_item("(a) The Tenant shall pay"),
            "- (a) The Tenant shall pay"
        );
        assert_eq!(
            format_list_item("(2) <u>Expiration</u> date"),
            "- (2) <u>Expiration</u> date"
        );
        assert_eq!(format_list_item("**a)** Bold label"), "- a) Bold label");
        // Russian lettered lists: "а)", "б)".
        assert_eq!(
            format_list_item("а) арендатор обязуется"),
            "- а) арендатор обязуется"
        );
        assert_eq!(format_list_item("б)арендодатель"), "- б) арендодатель");
        assert_eq!(format_list_item("2) Second"), "2) Second");
        assert_eq!(format_list_item("* Item"), "- Item");
    }

    #[test]
    fn format_list_item_leaves_text_that_only_looks_like_a_label() {
        for text in [
            "1.5 million was paid",
            "1.1 Scope",
            "e.g. the first case",
            "т.е. первое",
            "J.Smith signed",
            "J. W. Tukey suggested",
            "(c)(2) of this section",
            "(b).",
            "* * * * *",
        ] {
            assert_eq!(format_list_item(text), text);
            assert!(
                !starts_markdown_list_item(&format_list_item(text)),
                "{text}"
            );
        }
    }

    #[test]
    fn is_list_item_with_bullet_space() {
        assert!(is_list_item("● Item"));
        assert!(is_list_item("• Item"));
        assert!(is_list_item("- Item"));
    }
}
