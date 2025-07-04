use alloc::{borrow::Cow, vec, vec::Vec};
use core::iter::Peekable;

use ov6_user_lib::os_str::OsStr;

use crate::{
    command::{Command, OutputMode, Redirect},
    tokenizer::{Punct, Token, TokenizeError, Tokenizer},
};

static EXEC_TERMINATOR: &[Punct] = &[
    Punct::Pipe,
    Punct::AndAnd,
    Punct::OrOr,
    Punct::RParen,
    Punct::And,
    Punct::Semicolon,
];

macro_rules! try_opt {
    ($e:expr) => {
        match $e {
            Ok(Some(cmd)) => cmd,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        }
    };
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ParseError {
    #[error(transparent)]
    Tokenize(#[from] TokenizeError),
    #[error("leftovers '{0}'")]
    Leftovers(Token<'static>),
    #[error("missing file for redirection")]
    MissingFile,
    #[error("missing '{0}'")]
    MissingPunct(Punct),
    #[error("unexpected charactesr '{0}'")]
    UnexpectedPunct(Punct),
}

struct PeekTokenizer<'a> {
    tokens: Peekable<Tokenizer<'a>>,
}

impl<'a> PeekTokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            tokens: Tokenizer::new(input).peekable(),
        }
    }

    fn next(&mut self) -> Result<Option<Token<'a>>, TokenizeError> {
        self.tokens.next().transpose()
    }

    fn next_if<F>(&mut self, f: F) -> Result<Option<Token<'a>>, TokenizeError>
    where
        F: FnOnce(&Token<'a>) -> bool,
    {
        self.tokens
            .next_if(|t| match t {
                Ok(t) => f(t),
                Err(_e) => true,
            })
            .transpose()
    }

    #[expect(clippy::needless_pass_by_value)]
    fn next_if_eq<T>(&mut self, t: T) -> Result<Option<Token<'a>>, TokenizeError>
    where
        Token<'a>: PartialEq<T>,
    {
        self.next_if(|tt| *tt == t)
    }
}

pub struct Parser<'a> {
    tokens: PeekTokenizer<'a>,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            tokens: PeekTokenizer::new(input),
        }
    }

    pub fn parse(&mut self) -> Result<Vec<Command<'a>>, ParseError> {
        let list = self.parse_line()?;
        if let Some(token) = self.tokens.next()? {
            return Err(ParseError::Leftovers(token.into_owned()));
        }
        Ok(list)
    }

    fn parse_line(&mut self) -> Result<Vec<Command<'a>>, ParseError> {
        let mut list = vec![];
        while let Some(mut cmd) = self.parse_pipe()? {
            if self.tokens.next_if_eq(Punct::And)?.is_some() {
                cmd.background = true;
            }
            list.push(cmd);
            if self.tokens.next_if_eq(Punct::Semicolon)?.is_none() {
                break;
            }
        }
        Ok(list)
    }

    fn parse_pipe(&mut self) -> Result<Option<Command<'a>>, ParseError> {
        let mut cmd = try_opt!(self.parse_exec());
        if self.tokens.next_if_eq(Punct::Pipe)?.is_some() {
            cmd = Command::pipe(cmd, try_opt!(self.parse_pipe()));
        }
        if self.tokens.next_if_eq(Punct::AndAnd)?.is_some() {
            cmd = Command::logical_and(cmd, try_opt!(self.parse_pipe()));
        }
        if self.tokens.next_if_eq(Punct::OrOr)?.is_some() {
            cmd = Command::logical_or(cmd, try_opt!(self.parse_pipe()));
        }
        Ok(Some(cmd))
    }

    fn parse_redirs(&mut self, redirect: &mut Redirect<'a>) -> Result<(), ParseError> {
        loop {
            enum Redir {
                Stdin,
                Stdout(OutputMode),
            }
            let mode = if self.tokens.next_if_eq(Punct::Lt)?.is_some() {
                Redir::Stdin
            } else if self.tokens.next_if_eq(Punct::Gt)?.is_some() {
                Redir::Stdout(OutputMode::Truncate)
            } else if self.tokens.next_if_eq(Punct::GtGt)?.is_some() {
                Redir::Stdout(OutputMode::Append)
            } else {
                break;
            };
            let Some(Token::Str(file)) = self.tokens.next()? else {
                return Err(ParseError::MissingFile);
            };
            match mode {
                Redir::Stdin => redirect.stdin = Some(file),
                Redir::Stdout(mode) => redirect.stdout = Some((file, mode)),
            }
        }
        Ok(())
    }

    fn parse_subshell(&mut self) -> Result<Option<Command<'a>>, ParseError> {
        // LParen is consumed by caller
        let cmd = self.parse_line()?;
        if self.tokens.next_if_eq(Punct::RParen)?.is_none() {
            return Err(ParseError::MissingPunct(Punct::RParen));
        }
        let mut redirect = Redirect::new();
        self.parse_redirs(&mut redirect)?;
        Ok(Some(Command::subshell(cmd, redirect)))
    }

    fn parse_exec(&mut self) -> Result<Option<Command<'a>>, ParseError> {
        if self.tokens.next_if_eq(Punct::LParen)?.is_some() {
            return self.parse_subshell();
        }

        let mut argv = vec![];
        let mut redirect = Redirect::new();

        self.parse_redirs(&mut redirect)?;
        while let Some(tok) = self
            .tokens
            .next_if(|t| t.as_punct().is_none_or(|p| !EXEC_TERMINATOR.contains(&p)))?
        {
            let arg: Cow<OsStr> = match tok {
                Token::Str(Cow::Borrowed(arg)) => OsStr::new(arg).into(),
                Token::Str(Cow::Owned(arg)) => arg.into(),
                Token::Punct(p) => return Err(ParseError::UnexpectedPunct(p)),
            };
            argv.push(arg);
            self.parse_redirs(&mut redirect)?;
        }
        if argv.is_empty() {
            return Ok(None);
        }
        Ok(Some(Command::exec(argv, redirect)))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::command::CommandKind;

    #[track_caller]
    fn parse(input: &str) -> Result<Vec<Command<'_>>, ParseError> {
        Parser::new(input).parse()
    }

    #[track_caller]
    fn parse_ok<const N: usize>(input: &str) -> [Command<'_>; N] {
        parse(input).unwrap().try_into().unwrap()
    }

    #[track_caller]
    fn expect_redirect(
        redirect: &Redirect,
        expected_stdin: Option<&str>,
        expected_stdout: Option<(&str, OutputMode)>,
    ) {
        assert_eq!(redirect.stdin.as_deref(), expected_stdin.map(OsStr::new));
        assert_eq!(
            redirect.stdout.as_ref().map(|(s, m)| (s.as_ref(), *m)),
            expected_stdout.map(|(s, m)| (OsStr::new(s), m))
        );
    }

    #[track_caller]
    fn expect_subshell_common<'a, const N: usize>(
        cmd: Command<'a>,
        expected_stdin: Option<&str>,
        expected_stdout: Option<(&str, OutputMode)>,
        expected_background: bool,
    ) -> [Command<'a>; N] {
        let Command { kind, background } = cmd;
        let CommandKind::Subshell { list, redirect } = *kind else {
            panic!("Expected Subshell, found {kind:#?}")
        };
        expect_redirect(&redirect, expected_stdin, expected_stdout);
        assert_eq!(background, expected_background);
        list.try_into().unwrap()
    }

    #[track_caller]
    fn expect_subshell<const N: usize>(cmd: Command<'_>) -> [Command<'_>; N] {
        expect_subshell_common(cmd, None, None, false)
    }

    #[track_caller]
    fn expect_exec_common(
        cmd: Command,
        expected_argv: &[&str],
        expected_stdin: Option<&str>,
        expected_stdout: Option<(&str, OutputMode)>,
        expected_background: bool,
    ) {
        let Command { kind, background } = cmd;
        let CommandKind::Exec { argv, redirect } = *kind else {
            panic!("Expected Exec, found {kind:#?}");
        };
        let expected_argv = expected_argv
            .iter()
            .copied()
            .map(OsStr::new)
            .collect::<Vec<_>>();
        assert_eq!(&argv[..], &expected_argv[..]);
        expect_redirect(&redirect, expected_stdin, expected_stdout);
        assert_eq!(background, expected_background);
    }

    #[track_caller]
    fn expect_exec(cmd: Command, expected_argv: &[&str]) {
        expect_exec_common(cmd, expected_argv, None, None, false);
    }

    #[track_caller]
    fn expect_pipe(cmd: Command<'_>) -> (Command<'_>, Command<'_>) {
        let Command { kind, background } = cmd;
        let CommandKind::Pipe { left, right } = *kind else {
            panic!("Expected Pipe, found {kind:#?}")
        };
        assert!(!background);
        (left, right)
    }

    #[test]
    fn test_parse_empty() {
        let [] = parse_ok("");
        let [] = parse_ok("   ");
    }

    #[test]
    fn test_parse_simple_command() {
        let [cmd] = parse_ok("echo hello");
        expect_exec(cmd, &["echo", "hello"]);
    }

    #[test]
    fn test_parse_pipe() {
        let [cmd] = parse_ok("echo hello | grep h");
        let (left, right) = expect_pipe(cmd);
        expect_exec(left, &["echo", "hello"]);
        expect_exec(right, &["grep", "h"]);
    }

    #[test]
    fn test_parse_redirection() {
        let [cmd] = parse_ok("echo hello > output.txt");
        expect_exec_common(
            cmd,
            &["echo", "hello"],
            None,
            Some(("output.txt", OutputMode::Truncate)),
            false,
        );
    }

    #[test]
    fn test_parse_background() {
        let [cmd] = parse_ok("sleep 1 &");
        expect_exec_common(cmd, &["sleep", "1"], None, None, true);
    }

    #[test]
    fn test_parse_list() {
        let [cmd0, cmd1] = parse_ok("echo hello; echo world");
        expect_exec(cmd0, &["echo", "hello"]);
        expect_exec(cmd1, &["echo", "world"]);
    }

    #[test]
    fn test_parse_nested_commands() {
        let [cmd] = parse_ok("(echo hello; echo world) | grep h");
        let (cmd01, cmd2) = expect_pipe(cmd);
        let [cmd0, cmd1] = expect_subshell(cmd01);
        expect_exec(cmd0, &["echo", "hello"]);
        expect_exec(cmd1, &["echo", "world"]);
        expect_exec(cmd2, &["grep", "h"]);
    }

    #[test]
    fn test_parse_multiple_pipes() {
        let [cmd] = parse_ok("echo hello | grep h | wc -l");
        let (left, right) = expect_pipe(cmd);
        expect_exec(left, &["echo", "hello"]);
        let (right_left, right) = expect_pipe(right);
        expect_exec(right_left, &["grep", "h"]);
        expect_exec(right, &["wc", "-l"]);
    }

    #[test]
    fn test_parse_subshell() {
        let [cmd] = parse_ok("(echo hello)");
        let [subshell] = expect_subshell(cmd);
        expect_exec(subshell, &["echo", "hello"]);
    }

    #[test]
    fn test_parse_subshell_with_redirection() {
        let [cmd] = parse_ok("(echo hello) > output.txt");
        let [cmd] =
            expect_subshell_common(cmd, None, Some(("output.txt", OutputMode::Truncate)), false);
        expect_exec(cmd, &["echo", "hello"]);
    }

    #[test]
    fn test_parse_subshell_with_background() {
        let [cmd] = parse_ok("(echo hello) &");
        let [cmd] = expect_subshell_common(cmd, None, None, true);
        expect_exec(cmd, &["echo", "hello"]);
    }

    #[test]
    fn test_parse_multiple_commands_with_redirection() {
        let [cmd0, cmd1] = parse_ok("echo hello > output.txt; echo world >> output.txt");
        expect_exec_common(
            cmd0,
            &["echo", "hello"],
            None,
            Some(("output.txt", OutputMode::Truncate)),
            false,
        );
        expect_exec_common(
            cmd1,
            &["echo", "world"],
            None,
            Some(("output.txt", OutputMode::Append)),
            false,
        );
    }

    #[test]
    fn test_parse_complex_command() {
        let [cmd0, cmd1] = parse_ok("echo hello | grep h > output.txt; echo world &");
        let (pipe_left, pipe_right) = expect_pipe(cmd0);
        expect_exec(pipe_left, &["echo", "hello"]);
        expect_exec_common(
            pipe_right,
            &["grep", "h"],
            None,
            Some(("output.txt", OutputMode::Truncate)),
            false,
        );
        expect_exec_common(cmd1, &["echo", "world"], None, None, true);
    }
}
