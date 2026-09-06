//! Terminal capability probes for the surface.
//!
//! These are the only two facts about the environment a renderer needs before it
//! can decide what to emit: how wide, and whether colour would be read or logged.
//! Both are answers about the *stream being written to*, not about the process —
//! an interactive run with `2> transcript.log` wants a wide, colourless transcript
//! on stderr and a narrow, coloured answer on stdout.
//!
//! Cost matters: this runs on the startup path. `is_terminal()` is one `isatty` per
//! stream and `size()` is one `ioctl`; neither touches the network, the filesystem,
//! or Node.

use std::io::{IsTerminal, stderr, stdout};

/// `true` when stdout is a terminal, so stdout prose may be treated as something
/// a human is watching rather than a byte stream being captured.
pub fn stdout_is_terminal() -> bool {
  stdout().is_terminal()
}

/// `true` when stderr is a terminal, which is what decides whether transcript
/// chrome is worth colouring and wrapping at all.
pub fn stderr_is_terminal() -> bool {
  stderr().is_terminal()
}

/// Which stream a surface decision is being made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
  Stdout,
  Stderr,
}

impl Stream {
  /// `true` when this stream is attached to a terminal.
  pub fn is_terminal(self) -> bool {
    match self {
      Self::Stdout => stdout_is_terminal(),
      Self::Stderr => stderr_is_terminal(),
    }
  }

  /// Column width of the controlling terminal for this stream, `None` when the
  /// stream is not a terminal.
  ///
  /// `None` means "no idea, do not wrap", which is the correct behaviour for a
  /// pipe, a file, and a terminal whose size cannot be read. Substituting a
  /// guessed default would silently reflow a transcript the caller believed was
  /// unwrapped.
  pub fn width(self) -> Option<usize> {
    if !self.is_terminal() {
      return None;
    }
    let (columns, _rows) = crossterm::terminal::size().ok()?;
    let columns = usize::from(columns);
    (columns > 0).then_some(columns)
  }
}

/// Width of the transcript stream (stderr).
pub fn width() -> Option<usize> {
  Stream::Stderr.width()
}

/// Whether colour should be emitted, resolved from a caller's explicit choice.
///
/// `NO_COLOR` is honoured because it is the one convention with actual cross-tool
/// agreement; `TERM=dumb` is honoured because a terminal that cannot interpret
/// escapes renders them as text. An explicit `--color always` still wins over both:
/// the flag is the more recent, more specific statement of intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
  /// Colour when the destination is a terminal and the environment allows it.
  #[default]
  Auto,
  /// Colour regardless of destination or environment.
  Always,
  /// Never emit escape sequences.
  Never,
}

impl ColorChoice {
  /// Parse a `--color` value.
  pub fn parse(value: &str) -> Option<Self> {
    match value {
      "auto" => Some(Self::Auto),
      "always" | "yes" | "force" => Some(Self::Always),
      "never" | "no" | "none" => Some(Self::Never),
      _ => None,
    }
  }

  /// Resolve against the environment and the destination.
  pub fn resolve(self, destination_is_terminal: bool) -> bool {
    match self {
      Self::Always => true,
      Self::Never => false,
      Self::Auto => {
        destination_is_terminal && std::env::var_os("NO_COLOR").is_none() && !dumb_terminal()
      }
    }
  }
}

fn dumb_terminal() -> bool {
  matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}
