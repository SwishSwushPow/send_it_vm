//! sendit's own status, reported to the terminal with the Program Status
//! Protocol (OSC 7501): a terminal can then show that sendit is building a
//! base image, waits for an answer, or has failed, e.g. in a tab that isn't
//! in front.
//!
//! The reports carry their own `id`, so they leave the root record to the
//! programs in the VM. They go to stderr, and only when it is a terminal.
//! Nothing may report while a VM's console is attached: the guest's output
//! goes to the same terminal, and a report could land in the middle of one
//! of its escape sequences.

use std::io::{IsTerminal, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The longest `msg` the protocol allows, in bytes before encoding.
const MSG_LIMIT: usize = 2048;

pub enum Status<'a> {
    /// Busy, with a percentage if known.
    Working(&'a str, Option<u8>),
    /// Waiting for the answer to a question.
    Question(&'a str),
    /// Finished with results the user hasn't seen yet.
    Done(&'a str),
    /// Failed and stopped.
    Error(&'a str),
    /// Nothing to report (anymore).
    Clear,
}

impl Status<'_> {
    pub fn report(&self) {
        let mut stderr = std::io::stderr();
        if stderr.is_terminal() {
            let _ = stderr.write_all(self.sequence().as_bytes());
        }
    }

    fn sequence(&self) -> String {
        let (state, msg) = match *self {
            Status::Working(msg, _) => ("working", Some(msg)),
            Status::Question(msg) => ("blocked", Some(msg)),
            Status::Done(msg) => ("done", Some(msg)),
            Status::Error(msg) => ("error", Some(msg)),
            Status::Clear => ("clear", None),
        };
        let mut sequence = format!("\x1b]7501;state={state}:id=sendit");
        if msg.is_some() {
            sequence.push_str(":app=sendit");
        }
        match *self {
            Status::Working(_, Some(progress)) => {
                sequence.push_str(&format!(":progress={}", progress.min(100)));
            }
            Status::Question(_) => sequence.push_str(":kind=question"),
            _ => {}
        }
        if let Some(msg) = msg {
            sequence.push_str(":msg=");
            sequence.push_str(&STANDARD.encode(one_line(msg)));
        }
        sequence.push_str("\x1b\\");
        sequence
    }
}

/// `msg` on one line: control characters, such as line breaks, and runs of
/// whitespace become single spaces. Shortened to `MSG_LIMIT` bytes.
fn one_line(msg: &str) -> String {
    let spaced: String = msg
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut line = String::new();
    for c in spaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
    {
        if line.len() + c.len_utf8() > MSG_LIMIT {
            break;
        }
        line.push(c);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_reports() {
        // "Hi" in base64 is "SGk=".
        assert_eq!(
            Status::Working("Hi", None).sequence(),
            "\x1b]7501;state=working:id=sendit:app=sendit:msg=SGk=\x1b\\"
        );
        assert_eq!(
            Status::Working("Hi", Some(40)).sequence(),
            "\x1b]7501;state=working:id=sendit:app=sendit:progress=40:msg=SGk=\x1b\\"
        );
        assert_eq!(
            Status::Question("Hi").sequence(),
            "\x1b]7501;state=blocked:id=sendit:app=sendit:kind=question:msg=SGk=\x1b\\"
        );
        assert_eq!(
            Status::Done("Hi").sequence(),
            "\x1b]7501;state=done:id=sendit:app=sendit:msg=SGk=\x1b\\"
        );
        assert_eq!(
            Status::Error("Hi").sequence(),
            "\x1b]7501;state=error:id=sendit:app=sendit:msg=SGk=\x1b\\"
        );
        assert_eq!(
            Status::Clear.sequence(),
            "\x1b]7501;state=clear:id=sendit\x1b\\"
        );
    }

    #[test]
    fn keeps_messages_to_one_short_line() {
        assert_eq!(one_line(" a\nb\tc\x1b[1m \n"), "a b c [1m");
        assert_eq!(
            one_line("Share these?\n  ~ (read-write)"),
            "Share these? ~ (read-write)"
        );
        let long = "é".repeat(MSG_LIMIT);
        let line = one_line(&long);
        assert_eq!(line.len(), MSG_LIMIT);
        assert!(line.chars().all(|c| c == 'é'));
    }
}
