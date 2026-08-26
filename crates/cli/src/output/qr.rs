//! Printing the server-rendered QR code for TOTP enrollment.
//!
//! The server does the encoding, so `rc` carries no QR library and the console
//! and the CLI show the same symbol from the same source. All that happens here
//! is deciding whether a terminal can display it and writing it out.

use crate::output::Formatter;

/// Widest terminal the block art is worth attempting on.
///
/// A version-2 `otpauth://` QR is 25 modules plus a 4-module quiet zone on each
/// side, so 33 columns is the practical floor. Below that the symbol wraps and
/// becomes unscannable, and printing a broken one is worse than saying so.
const MIN_TERMINAL_COLUMNS: u16 = 33;

/// A version-2 symbol is 25 modules wide plus a 4-module quiet zone each side.
/// Lowering the floor below that would print a code no phone can read.
const _: () = assert!(
    MIN_TERMINAL_COLUMNS >= 25 + 4 + 4,
    "the QR floor must still fit a version-2 symbol with its quiet zone"
);

/// Print the QR, or explain why it was skipped.
///
/// Returns whether it was printed, so a caller can decide how loudly to point at
/// the manual key.
pub fn print_qr(formatter: &Formatter, qr_utf8: &str, suppressed: bool) -> bool {
    if suppressed || qr_utf8.is_empty() {
        return false;
    }

    if let Some((columns, _)) = terminal_size()
        && columns < MIN_TERMINAL_COLUMNS
    {
        formatter.println(&format!(
            "(Terminal is {columns} columns wide; the QR code needs at least {MIN_TERMINAL_COLUMNS}. Use the setup key below.)"
        ));
        return false;
    }

    formatter.println("");
    for line in qr_utf8.lines() {
        // The symbol is server-rendered, so it is escaped like any other server
        // text before it reaches a terminal. Block-drawing characters survive
        // untouched; an escape sequence smuggled in alongside them does not.
        formatter.println(&formatter.sanitize_text(line));
    }
    formatter.println("");
    true
}

fn terminal_size() -> Option<(u16, u16)> {
    console::Term::stdout()
        .size_checked()
        .map(|(rows, columns)| (columns, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputConfig;

    fn formatter() -> Formatter {
        Formatter::new(OutputConfig {
            no_color: true,
            ..Default::default()
        })
    }

    #[test]
    fn suppressing_the_qr_reports_that_nothing_was_printed() {
        assert!(!print_qr(&formatter(), "▀▄█", true));
    }

    #[test]
    fn an_empty_payload_prints_nothing() {
        // A server that omitted the field must not produce a blank frame the
        // user might mistake for an unscannable code.
        assert!(!print_qr(&formatter(), "", false));
    }
}
