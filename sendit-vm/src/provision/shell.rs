//! Splitting shell scripts into words, roughly, to find the scripts they
//! call with `sendit-provision` (see `scripts`).

use std::iter::Peekable;
use std::str::Chars;

/// The words of a shell script, roughly, each with the index of the line it
/// starts on. They are split at whitespace, with each newline and each of
/// the characters that end a command or redirect (`;&|()<>`) a word of its
/// own. A word keeps its quotes and backslashes, so that it can't pass for a
/// literal name, and a command substitution in it, `$(...)` or backticks,
/// which also adds the words in it, then its closing `)` or backtick, which
/// ends the command in it, as a word of its own. Arithmetic, `$((...))` or a
/// `((...))` command, is a word too. Comments and the bodies of
/// here-documents are left out.
pub(super) fn shell_words(text: &str) -> Vec<(usize, String)> {
    let mut lexer = Lexer {
        chars: text.chars().peekable(),
        line: 0,
        words: Vec::new(),
        heredocs: Vec::new(),
    };
    lexer.code(None);
    lexer.words
}

pub(super) fn is_operator(word: &str) -> bool {
    matches!(word, "\n" | ";" | "&" | "|" | "(" | ")" | "<" | ">" | "`")
}

/// Splits shell code into words for `shell_words`.
struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    /// The index of the line the next character is on.
    line: usize,
    words: Vec<(usize, String)>,
    /// The delimiters of the here-documents whose bodies follow the current
    /// line, each with whether its lines may be indented with tabs (`<<-`).
    heredocs: Vec<(String, bool)>,
}

impl Lexer<'_> {
    fn next(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn next_if(&mut self, expected: char) -> bool {
        let found = self.chars.peek() == Some(&expected);
        if found {
            self.next();
        }
        found
    }

    /// The index in `words` of the word being read, `current`, which starts
    /// on `line` if it is new. Taken before what comes in a word, so that a
    /// word comes before those of a command substitution in it.
    fn word(&mut self, current: &mut Option<usize>, line: usize) -> usize {
        *current.get_or_insert_with(|| {
            self.words.push((line, String::new()));
            self.words.len() - 1
        })
    }

    /// Reads code up to `close`, the end of a command substitution, or to the
    /// end of the text.
    fn code(&mut self, close: Option<char>) {
        let mut current = None;
        let mut depth: usize = 0;
        loop {
            let line = self.line;
            let Some(c) = self.next() else {
                return;
            };
            if close == Some(c) && (c == '`' || depth == 0) {
                return;
            }
            match c {
                '\n' => {
                    current = None;
                    self.words.push((line, "\n".into()));
                    self.heredoc_bodies();
                }
                c if c.is_whitespace() => current = None,
                '#' if current.is_none() => {
                    while self.chars.peek().is_some_and(|&c| c != '\n') {
                        self.next();
                    }
                }
                // A line continuation.
                '\\' if self.next_if('\n') => {}
                '\\' => {
                    let escaped = self.next();
                    let i = self.word(&mut current, line);
                    self.words[i].1.push('\\');
                    self.words[i].1.extend(escaped);
                }
                '\'' => {
                    let i = self.word(&mut current, line);
                    self.words[i].1.push('\'');
                    while let Some(c) = self.next() {
                        self.words[i].1.push(c);
                        if c == '\'' {
                            break;
                        }
                    }
                }
                '"' => {
                    let i = self.word(&mut current, line);
                    self.words[i].1.push('"');
                    self.double_quotes(i);
                }
                // ANSI-C quotes, in which a backslash escapes an apostrophe.
                '$' if self.next_if('\'') => {
                    let i = self.word(&mut current, line);
                    self.words[i].1.push_str("$'");
                    while let Some(c) = self.next() {
                        self.words[i].1.push(c);
                        match c {
                            '\\' => {
                                let escaped = self.next();
                                self.words[i].1.extend(escaped);
                            }
                            '\'' => break,
                            _ => {}
                        }
                    }
                }
                '$' if self.next_if('(') => {
                    let i = self.word(&mut current, line);
                    self.dollar_paren(i);
                }
                '`' => {
                    let i = self.word(&mut current, line);
                    self.substitution(i, '`');
                }
                // An arithmetic command; `( (...) )` is a subshell in one.
                '(' if self.next_if('(') => {
                    current = None;
                    let i = self.word(&mut current, line);
                    self.arithmetic(i, "((");
                    current = None;
                }
                '<' if self.next_if('<') => {
                    current = None;
                    self.words.push((line, "<".into()));
                    if !self.next_if('<') {
                        self.heredoc();
                    }
                }
                ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                    current = None;
                    match c {
                        '(' => depth += 1,
                        ')' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    self.words.push((line, c.into()));
                }
                c => {
                    let i = self.word(&mut current, line);
                    self.words[i].1.push(c);
                }
            }
        }
    }

    /// Reads the rest of a double-quoted string into `words[i]`, and the
    /// command substitutions in it as code.
    fn double_quotes(&mut self, i: usize) {
        while let Some(c) = self.next() {
            match c {
                '"' => {
                    self.words[i].1.push('"');
                    return;
                }
                '\\' => {
                    self.words[i].1.push('\\');
                    let escaped = self.next();
                    self.words[i].1.extend(escaped);
                }
                '$' if self.next_if('(') => self.dollar_paren(i),
                '`' => self.substitution(i, '`'),
                c => self.words[i].1.push(c),
            }
        }
    }

    /// Reads what follows `$(` into `words[i]`: arithmetic if another `(`
    /// follows right away, otherwise a command substitution.
    fn dollar_paren(&mut self, i: usize) {
        if self.next_if('(') {
            self.arithmetic(i, "$((");
        } else {
            self.substitution(i, ')');
        }
    }

    /// Reads arithmetic that starts with `open`, `$((` or `((`, up to the
    /// matching `))` into `words[i]`, and the command substitutions in it as
    /// code. A `<<` in it is a shift, not a here-document.
    fn arithmetic(&mut self, i: usize, open: &str) {
        self.words[i].1.push_str(open);
        let mut depth: usize = 0;
        while let Some(c) = self.next() {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 && self.next_if(')') => {
                    self.words[i].1.push_str("))");
                    return;
                }
                ')' => depth = depth.saturating_sub(1),
                '$' if self.next_if('(') => {
                    self.dollar_paren(i);
                    continue;
                }
                '`' => {
                    self.substitution(i, '`');
                    continue;
                }
                _ => {}
            }
            self.words[i].1.push(c);
        }
    }

    /// Reads a command substitution that ends with `close` as code, and
    /// marks it in `words[i]`.
    fn substitution(&mut self, i: usize, close: char) {
        self.words[i]
            .1
            .push_str(if close == ')' { "$(" } else { "`" });
        self.code(Some(close));
        self.words.push((self.line, close.into()));
        self.words[i].1.push(close);
    }

    /// Notes the here-document after `<<`, whose body follows the line.
    fn heredoc(&mut self) {
        let tabs = self.next_if('-');
        while self.chars.peek().is_some_and(|&c| c == ' ' || c == '\t') {
            self.next();
        }
        // Quotes and backslashes only say that the body is literal.
        let mut delimiter = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || ";&|()<>".contains(c) {
                break;
            }
            self.next();
            if !"'\"\\".contains(c) {
                delimiter.push(c);
            }
        }
        self.heredocs.push((delimiter, tabs));
    }

    /// Skips the bodies of the here-documents noted on the line that just
    /// ended. Commands in an unquoted body's substitutions run too, but are
    /// rare enough to be left out.
    fn heredoc_bodies(&mut self) {
        for (delimiter, tabs) in std::mem::take(&mut self.heredocs) {
            loop {
                let mut line = String::new();
                while let Some(c) = self.next() {
                    if c == '\n' {
                        break;
                    }
                    line.push(c);
                }
                let line = if tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line.trim_end_matches('\r') == delimiter || self.chars.peek().is_none() {
                    break;
                }
            }
        }
    }
}
