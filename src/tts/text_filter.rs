// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/text_filter.rs
//! Pre-synthesis Markdown stripper. The LLM emits visual formatting
//! (`**bold**`, `# heading`, `[text](url)`, fenced code) that all current
//! backends — Piper/eSpeak via espeak-ng phonetics and OpenAI's text-faithful
//! TTS alike — read out literally as "asterisk asterisk bold asterisk
//! asterisk". This filter removes the formatting markers while preserving
//! the spoken content, so callers (the speak / speak_stream entry points
//! in `service.rs`) can hand the stripped text straight to the backend.
//!
//! Emoji are intentionally left untouched — the engines pronounce them by
//! Unicode name ("smiling face") which is verbose but not as jarring as
//! "asterisk asterisk".

/// Strip Markdown formatting that TTS engines verbalize literally.
/// Removes fenced code blocks, headings, blockquote/list markers,
/// emphasis delimiters (`**`, `__`, `*`, `_`, `~~`), inline code
/// backticks, and link/image syntax (keeping the link/alt text).
pub fn strip_markdown_for_speech(input: &str) -> String {
    let no_fences = strip_fenced_code(input);

    let mut out = String::with_capacity(no_fences.len());
    let mut first = true;
    for line in no_fences.lines() {
        if !first { out.push('\n'); }
        first = false;
        let l = strip_line_markers(line);
        let l = unwrap_inline_markup(&l);
        out.push_str(&l);
    }
    out
}

fn strip_fenced_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_fence = false;
    let mut first = true;
    for line in s.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            if !first { out.push('\n'); }
            first = false;
            out.push_str(line);
        }
    }
    out
}

fn strip_line_markers(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
    {
        return String::new();
    }

    let s = line.trim_start();
    if let Some(rest) = strip_heading(s) {
        return rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("> ") {
        return rest.to_string();
    }
    if s == ">" {
        return String::new();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            return rest.to_string();
        }
    }
    if let Some(rest) = strip_numbered(s) {
        return rest.to_string();
    }
    s.to_string()
}

fn strip_heading(s: &str) -> Option<&str> {
    let mut count = 0;
    let bytes = s.as_bytes();
    while count < bytes.len() && bytes[count] == b'#' {
        count += 1;
        if count > 6 { return None; }
    }
    if count == 0 { return None; }
    let rest = &s[count..];
    if let Some(stripped) = rest.strip_prefix(' ') {
        Some(stripped)
    } else if rest.is_empty() {
        Some("")
    } else {
        None
    }
}

fn strip_numbered(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
    if i == 0 || i > 9 { return None; }
    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1] == b' ' {
        Some(&s[i + 2..])
    } else {
        None
    }
}

fn unwrap_inline_markup(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '`' { j += 1; }
                if j < chars.len() {
                    out.extend(chars[i + 1..j].iter());
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => {
                if let Some((alt, end)) = parse_link(&chars, i + 1) {
                    out.push_str(&alt);
                    i = end;
                } else {
                    i += 1;
                }
            }
            '[' => {
                if let Some((text, end)) = parse_link(&chars, i) {
                    out.push_str(&text);
                    i = end;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '*' | '_' | '~' => {
                let mut j = i;
                while j < chars.len() && chars[j] == c && j - i < 3 { j += 1; }
                i = j;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn parse_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'[') { return None; }
    let mut i = start + 1;
    let mut text = String::new();
    while i < chars.len() && chars[i] != ']' {
        text.push(chars[i]);
        i += 1;
    }
    if i >= chars.len() || chars.get(i + 1) != Some(&'(') { return None; }
    let mut depth = 1;
    let mut j = i + 2;
    while j < chars.len() {
        match chars[j] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 { return Some((text, j + 1)); }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bold_and_italic() {
        assert_eq!(strip_markdown_for_speech("This is **bold** and *italic*"), "This is bold and italic");
        assert_eq!(strip_markdown_for_speech("__strong__ _emph_"), "strong emph");
    }

    #[test]
    fn strips_strikethrough() {
        assert_eq!(strip_markdown_for_speech("~~gone~~ here"), "gone here");
    }

    #[test]
    fn strips_inline_code() {
        assert_eq!(strip_markdown_for_speech("run `cargo build` now"), "run cargo build now");
    }

    #[test]
    fn drops_fenced_code_block() {
        let md = "before\n```rust\nfn main() {}\n```\nafter";
        assert_eq!(strip_markdown_for_speech(md), "before\nafter");
    }

    #[test]
    fn unwraps_links_and_images() {
        assert_eq!(strip_markdown_for_speech("see [the docs](https://example.com)"), "see the docs");
        assert_eq!(strip_markdown_for_speech("![cat](cat.png) is cute"), "cat is cute");
    }

    #[test]
    fn strips_headings_and_quotes() {
        assert_eq!(strip_markdown_for_speech("# Title"), "Title");
        assert_eq!(strip_markdown_for_speech("### Sub"), "Sub");
        assert_eq!(strip_markdown_for_speech("> quoted"), "quoted");
    }

    #[test]
    fn strips_list_markers() {
        let md = "- one\n- two\n* three\n+ four\n1. five\n2. six";
        let want = "one\ntwo\nthree\nfour\nfive\nsix";
        assert_eq!(strip_markdown_for_speech(md), want);
    }

    #[test]
    fn drops_horizontal_rules() {
        let md = "above\n---\nbelow";
        assert_eq!(strip_markdown_for_speech(md), "above\n\nbelow");
    }

    #[test]
    fn preserves_emoji() {
        assert_eq!(strip_markdown_for_speech("hello 😊 world"), "hello 😊 world");
    }

    #[test]
    fn leaves_plain_text_intact() {
        let plain = "Just a normal sentence with no markdown.";
        assert_eq!(strip_markdown_for_speech(plain), plain);
    }

    #[test]
    fn handles_unmatched_delimiters_gracefully() {
        // Lone `*` and unclosed backtick — drop the marker, keep the body.
        assert_eq!(strip_markdown_for_speech("a * b"), "a  b");
        assert_eq!(strip_markdown_for_speech("oops `unclosed"), "oops unclosed");
    }
}
