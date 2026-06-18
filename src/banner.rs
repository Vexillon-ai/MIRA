// SPDX-License-Identifier: AGPL-3.0-or-later

//! Startup banner for text-mode MIRA surfaces (the `--simple` CLI and any
//! channel that prints a header). The version is pulled from `CARGO_PKG_VERSION`
//! so bumping `Cargo.toml` is the only step needed to refresh what users see.

/// Render the MIRA banner. `mode` is a short label ("simple", "cli", …)
/// that appears next to the version in the bottom rule.
pub fn render(mode: &str) -> String {
    const INSIDE: usize = 44;
    let version = env!("CARGO_PKG_VERSION");
    let label   = format!(" v{} · {} ", version, mode);

    // Right-anchor the label with a short trailing stub so it reads like a
    // newspaper nameplate; the leading rule carries most of the width.
    let label_len = label.chars().count();
    let budget    = INSIDE.saturating_sub(label_len);
    let trailing  = budget.min(7);
    let leading   = budget.saturating_sub(trailing);

    let mut out = String::new();
    out.push_str(&format!("╭{}╮\n", "─".repeat(INSIDE)));
    out.push_str(&format!("│{:^1$}│\n", "",                                      INSIDE));
    out.push_str(&format!("│{:^1$}│\n", "M  I  R  A",                            INSIDE));
    out.push_str(&format!("│{:^1$}│\n", "",                                      INSIDE));
    out.push_str(&format!("│{:^1$}│\n", "Multi-tasking Intelligent Responsive",  INSIDE));
    out.push_str(&format!("│{:^1$}│\n", "Assistant",                             INSIDE));
    out.push_str(&format!("│{:^1$}│\n", "",                                      INSIDE));
    out.push_str(&format!("╰{}{}{}╯", "─".repeat(leading), label, "─".repeat(trailing)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_current_version_and_mode() {
        let out = render("simple");
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("simple"));
        assert!(out.contains("M  I  R  A"));
    }

    #[test]
    fn every_line_has_equal_display_width() {
        // All frame lines should be the same length (in chars) so the box
        // aligns in any monospace font. Version strings of varying length
        // must not break this invariant.
        let out = render("simple");
        let widths: Vec<usize> = out.lines().map(|l| l.chars().count()).collect();
        let first = widths[0];
        for (i, w) in widths.iter().enumerate() {
            assert_eq!(*w, first, "line {} has width {}, expected {}", i, w, first);
        }
    }

    #[test]
    fn handles_long_mode_label() {
        // Defensive: longer labels should still render without overflowing.
        let out = render("verbose-diagnostic-mode");
        for line in out.lines() {
            assert!(line.chars().count() >= 40, "line too short: {:?}", line);
        }
    }
}
