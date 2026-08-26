//! The yes/no prompt for an action worth asking about twice.
//!
//! Three commands had grown their own copy of this: deleting an OIDC provider,
//! running a replication check that writes to every target, and clearing
//! somebody's second factor. They differed only in their three strings, while
//! agreeing on the parts that matter — that `--yes` skips the question, that a
//! run with nobody to ask fails instead of assuming consent, and that anything
//! other than `y`/`yes` is a decline rather than a default.
//!
//! Those are the rules a fourth copy would be most likely to get subtly wrong,
//! so they live here once.

use std::io::{BufRead as _, IsTerminal as _, Write as _};

use rc_core::{Error, Result};

use crate::output::Formatter;

/// What to say while confirming one particular action.
pub(crate) struct Confirmation<'a> {
    /// The question, ending in `[y/N]`.
    ///
    /// Callers that interpolate a name are responsible for passing it through
    /// [`Formatter::sanitize_text`] first: it reaches a terminal from here with
    /// no further escaping.
    pub(crate) prompt: &'a str,
    /// Why `--yes` is required when there is no terminal to ask on.
    pub(crate) requires_yes: &'a str,
    /// Reported when the answer is anything but yes.
    pub(crate) declined: &'a str,
}

/// Ask, unless `yes` was passed.
///
/// `Ok(())` means go ahead. A refusal is [`Error::Interrupted`]; a run that
/// could not ask at all is [`Error::InvalidPath`], which is a usage problem and
/// exits as one.
pub(crate) fn confirm(request: &Confirmation<'_>, yes: bool, formatter: &Formatter) -> Result<()> {
    if yes {
        return Ok(());
    }
    // Refuse rather than proceed: a machine-readable run has nobody to answer,
    // and treating silence as consent is how a destructive command becomes a
    // surprise in someone's CI log.
    if formatter.is_json() || !std::io::stdin().is_terminal() {
        return Err(Error::InvalidPath(request.requires_yes.to_string()));
    }

    // The question goes to stderr so stdout stays usable in a pipeline.
    let mut stderr = std::io::stderr().lock();
    write!(stderr, "{} ", request.prompt).map_err(Error::Io)?;
    stderr.flush().map_err(Error::Io)?;

    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(Error::Io)?;

    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(Error::Interrupted(request.declined.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputConfig;

    fn request() -> Confirmation<'static> {
        Confirmation {
            prompt: "Delete everything? [y/N]",
            requires_yes: "Deleting everything requires --yes in non-interactive or JSON mode",
            declined: "Deleting everything was declined",
        }
    }

    fn formatter(json: bool) -> Formatter {
        Formatter::new(OutputConfig {
            json,
            no_color: true,
            ..Default::default()
        })
    }

    #[test]
    fn yes_skips_the_question_entirely() {
        // True even in JSON mode, where there would be nobody to ask.
        confirm(&request(), true, &formatter(true)).expect("--yes must be honoured");
    }

    #[test]
    fn json_mode_refuses_instead_of_assuming_consent() {
        let error = confirm(&request(), false, &formatter(true)).expect_err("must refuse");

        assert!(matches!(error, Error::InvalidPath(_)), "{error:?}");
        assert_eq!(error.exit_code(), 2, "a missing --yes is a usage error");
        assert!(error.to_string().contains("--yes"), "{error}");
    }

    #[test]
    fn a_declined_answer_is_reported_as_an_interruption() {
        // Not asserted through the prompt, which needs a terminal: this pins the
        // contract the callers rely on for their exit code.
        let error = Error::Interrupted(request().declined.to_string());
        assert_eq!(error.exit_code(), 130);
    }
}
