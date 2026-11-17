use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use super::{Conversation, PolkitError, PolkitMessageKind, PolkitResult};

// polkit-agent-helper-1 writes one line per step, and reads one back for each prompt
const SUCCESS: &str = "SUCCESS";
const FAILURE: &str = "FAILURE";
const PROMPT_ECHO_OFF: &str = "PAM_PROMPT_ECHO_OFF ";
const PROMPT_ECHO_ON: &str = "PAM_PROMPT_ECHO_ON ";
const TEXT_INFO: &str = "PAM_TEXT_INFO ";
const ERROR_MSG: &str = "PAM_ERROR_MSG ";

/*
 * starts the helper for one user and answers its PAM conversation; the
 * helper tells polkitd the result itself, so nothing goes back from here
 */
pub(super) fn authenticate(
    conversation: &Arc<Conversation>,
    helper: &Path,
    user: &str,
    cookie: &str,
) -> PolkitResult {
    let mut child = match Command::new(helper)
        .arg(user)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,

        Err(error) => return PolkitResult::Error(PolkitError::Worker(error.to_string())),
    };

    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        stop(&mut child);

        return PolkitResult::Error(PolkitError::Worker(String::from(
            "the helper has no pipes to talk through",
        )));
    };

    // the helper answers this one cookie and refuses anything else
    if line(&mut stdin, cookie).is_err() {
        stop(&mut child);

        return PolkitResult::Error(PolkitError::Worker(String::from(
            "the helper did not take the cookie",
        )));
    }

    let mut reader = BufReader::new(stdout);
    let mut text = String::new();

    loop {
        if conversation.aborted() {
            stop(&mut child);

            return PolkitResult::Cancelled;
        }

        text.clear();

        match reader.read_line(&mut text) {
            Ok(0) => {
                stop(&mut child);

                return PolkitResult::Error(PolkitError::Worker(String::from(
                    "the helper stopped without an answer",
                )));
            }

            Ok(_) => {}

            Err(error) => {
                stop(&mut child);

                return PolkitResult::Error(PolkitError::Worker(error.to_string()));
            }
        }

        let text = text.trim_end();

        if text == SUCCESS || text == FAILURE {
            let result = if text == SUCCESS {
                PolkitResult::Success
            } else {
                PolkitResult::Rejected
            };

            // its last word has been said, so closing stdin lets it exit
            drop(stdin);

            let _ = child.wait();

            return result;
        }

        let step = if let Some(prompt) = text.strip_prefix(PROMPT_ECHO_OFF) {
            Some((prompt, PolkitMessageKind::Prompt { visible: false }))
        } else if let Some(prompt) = text.strip_prefix(PROMPT_ECHO_ON) {
            Some((prompt, PolkitMessageKind::Prompt { visible: true }))
        } else if let Some(info) = text.strip_prefix(TEXT_INFO) {
            Some((info, PolkitMessageKind::Info))
        } else {
            text.strip_prefix(ERROR_MSG)
                .map(|error| (error, PolkitMessageKind::Error))
        };

        // a line this protocol has no meaning for is left for the helper to finish with
        let Some((asked, kind)) = step else {
            continue;
        };

        let answer = match super::message(conversation, String::from(asked), kind) {
            Ok(answer) => answer,

            Err(()) => {
                stop(&mut child);

                // the service drops a prompt only to cancel it
                return if conversation.aborted() {
                    PolkitResult::Cancelled
                } else {
                    PolkitResult::Error(PolkitError::Worker(String::from(
                        "the prompt stopped waiting for an answer",
                    )))
                };
            }
        };

        let Some(answer) = answer else {
            continue;
        };

        if line(&mut stdin, &answer).is_err() {
            stop(&mut child);

            return PolkitResult::Error(PolkitError::Worker(String::from(
                "the helper stopped listening",
            )));
        }
    }
}

// one answer per line, flushed, because the helper reads as it asks
fn line(stdin: &mut impl Write, text: &str) -> std::io::Result<()> {
    writeln!(stdin, "{text}")?;

    stdin.flush()
}

// a helper that will not exit on its own is killed
fn stop(child: &mut Child) {
    let _ = child.kill();

    let _ = child.wait();
}
