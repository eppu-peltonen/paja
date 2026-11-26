/// Tell the person watching what `paja run` is doing now: one line on stderr, so stdout keeps
/// its one JSON value per item. A long step says so, since silence reads as broken.
/// Text from outside (an issue title, a verdict) can carry terminal escapes or line breaks
/// that would fake or rewrite earlier lines, so control characters become spaces and a long
/// message is cut.
pub fn progress(message: &str) {
    use std::io::Write;
    // A closed stderr must never abort a run halfway, for example between merge and record.
    let _ = writeln!(std::io::stderr(), "paja: {}", shown_line(message));
}

/// Characters that can break, reorder or hide text on a terminal: controls, line and
/// paragraph separators, bidi overrides and isolates, and zero-width marks.
pub(super) fn unsafe_on_a_terminal(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

pub(super) fn shown_line(message: &str) -> String {
    const MAX: usize = 300;
    let mut line: String = message
        .chars()
        .map(|c| if unsafe_on_a_terminal(c) { ' ' } else { c })
        .take(MAX)
        .collect();
    if message.chars().count() > MAX {
        line.push_str("...");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines_cannot_carry_terminal_controls() {
        let line = shown_line("Fix\r\x1b[2Kpaja: landed #5\nnext\u{2028}\u{202E}x\u{2067}y");
        assert!(!line.chars().any(unsafe_on_a_terminal), "{line:?}");
        assert!(shown_line(&"x".repeat(1000)).chars().count() <= 303);
    }
}
