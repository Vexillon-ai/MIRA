// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/markdown.rs
//
// Detects markdown table blocks inside LLM response text and reformats them
// with proper column alignment so they render cleanly in the TUI.

/// A single processed line from message content.
#[derive(Debug, Clone)]
pub enum ContentLine {
    /// Ordinary text — render as-is.
    Plain(String),
    /// Top border: ┌────┬────┐
    TableTop { col_widths: Vec<usize> },
    /// A table data row (including the header row).
    TableRow {
        cells:      Vec<String>,
        col_widths: Vec<usize>,
        is_header:  bool,
    },
    /// The header/body separator: ├────┼────┤
    TableSep { col_widths: Vec<usize> },
    /// Bottom border: └────┴────┘
    TableBottom { col_widths: Vec<usize> },
}

/// Returns `true` when the line looks like a markdown table row.
fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    t.contains('|') && !t.is_empty()
}

/// Returns `true` for separator rows like `|---|:---|---:|`.
fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.contains('|')
        && t.contains('-')
        && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Split a row on `|`, stripping the leading/trailing `|` if present.
fn parse_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Walk `content`, identify contiguous table blocks, and return typed lines.
pub fn process_content(content: &str) -> Vec<ContentLine> {
    let raw: Vec<&str> = content.lines().collect();
    let n = raw.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;

    while i < n {
        // Collect a run of consecutive table lines
        if is_table_line(raw[i]) {
            let start = i;
            while i < n && is_table_line(raw[i]) {
                i += 1;
            }
            let block = &raw[start..i];
            out.extend(format_table_block(block));
        } else {
            out.push(ContentLine::Plain(raw[i].to_string()));
            i += 1;
        }
    }
    out
}

/// Format a slice of raw table lines into `ContentLine` values with
/// columns padded to uniform widths.
fn format_table_block(block: &[&str]) -> Vec<ContentLine> {
    // Separate data rows from the separator row(s)
    let data_rows: Vec<Vec<String>> = block
        .iter()
        .filter(|l| !is_separator(l))
        .map(|l| parse_cells(l))
        .collect();

    if data_rows.is_empty() {
        return block.iter().map(|l| ContentLine::Plain(l.to_string())).collect();
    }

    // Column count = widest row
    let col_count = data_rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return block.iter().map(|l| ContentLine::Plain(l.to_string())).collect();
    }

    // Compute uniform column widths from all data rows
    let mut col_widths = vec![3usize; col_count]; // minimum 3 chars
    for row in &data_rows {
        for (j, cell) in row.iter().enumerate().take(col_count) {
            col_widths[j] = col_widths[j].max(cell.len());
        }
    }

    let has_sep_row = block.iter().any(|l| is_separator(l));

    let mut lines = Vec::new();
    // ┌────┬────┐
    lines.push(ContentLine::TableTop { col_widths: col_widths.clone() });

    for (row_idx, row) in data_rows.iter().enumerate() {
        let is_header = row_idx == 0;
        let mut padded = Vec::with_capacity(col_count);
        for j in 0..col_count {
            padded.push(row.get(j).map(|s| s.as_str()).unwrap_or("").to_string());
        }
        lines.push(ContentLine::TableRow {
            cells: padded,
            col_widths: col_widths.clone(),
            is_header,
        });
        // ├────┼────┤ after header
        if is_header && (has_sep_row || data_rows.len() > 1) {
            lines.push(ContentLine::TableSep { col_widths: col_widths.clone() });
        }
    }

    // └────┴────┘
    lines.push(ContentLine::TableBottom { col_widths: col_widths.clone() });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_table() {
        let content = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = process_content(content);
        assert!(matches!(lines[0], ContentLine::TableTop { .. }));
        assert!(matches!(lines[1], ContentLine::TableRow { is_header: true, .. }));
        assert!(matches!(lines[2], ContentLine::TableSep { .. }));
        assert!(matches!(lines[3], ContentLine::TableRow { is_header: false, .. }));
        assert!(matches!(lines[4], ContentLine::TableBottom { .. }));
    }

    #[test]
    fn test_plain_text_passthrough() {
        let content = "Hello world\nSecond line";
        let lines = process_content(content);
        assert_eq!(lines.len(), 2);
        assert!(matches!(lines[0], ContentLine::Plain(_)));
    }

    #[test]
    fn test_mixed_content() {
        let content = "Some text\n| A | B |\n|---|---|\n| 1 | 2 |\nMore text";
        let lines = process_content(content);
        assert!(matches!(lines[0], ContentLine::Plain(_)));
        assert!(matches!(lines[1], ContentLine::TableTop { .. }));
        assert!(matches!(lines[2], ContentLine::TableRow { is_header: true, .. }));
        assert!(matches!(lines[lines.len()-1], ContentLine::Plain(_)));
    }

    #[test]
    fn test_column_widths_max() {
        let content = "| Short | A very long header |\n|---|---|\n| x | y |";
        let lines = process_content(content);
        if let ContentLine::TableRow { col_widths, .. } = &lines[0] {
            assert!(col_widths[1] >= "A very long header".len());
        }
    }
}
