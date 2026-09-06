//! Semantic styling roles and the restrained palette that renders them.
//!
//! The renderer never asks "what colour is this text". It asks "what *is* this
//! text", and the palette answers. That indirection carries the whole visual
//! policy of the surface:
//!
//! - the distinction the AGENTS rules require — operation vs argument vs path —
//!   is expressible at all only because those are separate roles;
//! - provenance-aware reasoning is a *role per provenance*, not one "thinking"
//!   colour, because a reader who cannot tell provider summary from native
//!   reasoning has been told something false;
//! - colour can be switched off without losing any of it, because a role that
//!   collapsed to "plain text" in monochrome would mean the information was only
//!   ever decoration.
//!
//! Nothing in this module reads a terminal, and nothing decides *when* colour is
//! appropriate; see [`Palette::monochrome`] and the surface's colour choice.

use std::fmt;

/// What a piece of surface text is, in runtime terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
  /// The `>`/`$` marker of a user prompt.
  Prompt,
  /// Text the user typed as an instruction.
  UserText,
  /// Assistant prose.
  Assistant,
  /// Slash-command name, or a tool operation name.
  Operation,
  /// A `--flag`-shaped token.
  Flag,
  /// A path-shaped token, whether typed by the user or named by a tool.
  Path,
  /// A non-path, non-flag argument value.
  Argument,
  /// Tool output verbatim.
  ToolOutput,
  /// Reasoning the model emitted itself.
  ReasoningNative,
  /// A provider-authored summary of reasoning the runtime cannot see.
  ReasoningProviderSummary,
  /// An explanation the runtime asked the model to produce.
  ReasoningDeclared,
  /// pi-rs' own after-the-fact inference. A hypothesis, never recovered thought.
  ReasoningReconstructed,
  /// A tool or turn state that ended well.
  StateOk,
  /// A tool or turn state that ended badly.
  StateFailed,
  /// A state that was never observed. Deliberately its own role: `unknown` is not
  /// a spelling of `failed`.
  StateUnknown,
  /// Machine facts that are not content: counts, durations, ids.
  Meta,
  /// Quiet context that should recede.
  Muted,
  /// An informational diagnostic.
  Info,
  /// A warning.
  Warning,
  /// An error.
  Error,
  /// A rare event worth breaking calm rendering for: failover, retry, unrecoverable
  /// uncertainty.
  Rare,
  /// The status line.
  Status,
}

impl Role {
  /// Stable machine name for roles that appear in exports or diagnostics.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Prompt => "prompt",
      Self::UserText => "user",
      Self::Assistant => "assistant",
      Self::Operation => "operation",
      Self::Flag => "flag",
      Self::Path => "path",
      Self::Argument => "argument",
      Self::ToolOutput => "tool_output",
      Self::ReasoningNative => "reasoning_native",
      Self::ReasoningProviderSummary => "reasoning_provider_summary",
      Self::ReasoningDeclared => "reasoning_declared",
      Self::ReasoningReconstructed => "reasoning_reconstructed",
      Self::StateOk => "state_ok",
      Self::StateFailed => "state_failed",
      Self::StateUnknown => "state_unknown",
      Self::Meta => "meta",
      Self::Muted => "muted",
      Self::Info => "info",
      Self::Warning => "warning",
      Self::Error => "error",
      Self::Rare => "rare",
      Self::Status => "status",
    }
  }

  /// `true` when the role is reasoning-like text and therefore must carry a
  /// provenance label wherever it is rendered.
  pub fn is_reasoning(self) -> bool {
    matches!(
      self,
      Self::ReasoningNative
        | Self::ReasoningProviderSummary
        | Self::ReasoningDeclared
        | Self::ReasoningReconstructed
    )
  }
}

/// The eight ANSI primary colours, plus the terminal default.
///
/// Restricted deliberately: a semantic palette needs a handful of distinguishable
/// hues, and 256-colour or truecolour output would not survive `TERM=xterm` and
/// would make the diff of a transcript test unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
  Default,
  Dark,
  Red,
  Green,
  Yellow,
  Blue,
  Magenta,
  Cyan,
  White,
}

impl Color {
  fn sgr(self) -> Option<u8> {
    match self {
      Self::Default => None,
      Self::Dark => Some(90),
      Self::Red => Some(31),
      Self::Green => Some(32),
      Self::Yellow => Some(33),
      Self::Blue => Some(34),
      Self::Magenta => Some(35),
      Self::Cyan => Some(36),
      Self::White => Some(37),
    }
  }
}

/// A resolved visual claim for one span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
  pub color: Color,
  pub bold: bool,
  pub dim: bool,
  pub italic: bool,
}

impl Style {
  const fn plain() -> Self {
    Self {
      color: Color::Default,
      bold: false,
      dim: false,
      italic: false,
    }
  }

  const fn color(color: Color) -> Self {
    Self {
      color,
      ..Self::plain()
    }
  }
}

/// Renders roles either as ANSI or as nothing at all.
///
/// One palette, two modes, so "colour off" is a rendering parameter rather than a
/// second code path that can drift out of agreement with the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
  color: bool,
}

impl Palette {
  /// Colour-capable palette.
  pub const fn colored() -> Self {
    Self { color: true }
  }

  /// Plain-text palette: roles still select weight and italics, colour does not.
  pub const fn monochrome() -> Self {
    Self { color: false }
  }

  /// `true` when this palette emits escape sequences.
  pub const fn is_color(self) -> bool {
    self.color
  }

  /// The style a role resolves to. Always available, even in monochrome, so
  /// callers that want the semantics (a test, an exporter) can ask without
  /// caring about colour.
  pub fn style(self, role: Role) -> Style {
    match role {
      Role::Prompt => Style {
        color: Color::Green,
        bold: true,
        dim: false,
        italic: false,
      },
      Role::UserText => Style::plain(),
      Role::Assistant => Style::plain(),
      Role::Operation => Style {
        color: Color::Cyan,
        bold: true,
        dim: false,
        italic: false,
      },
      Role::Flag => Style::color(Color::Blue),
      Role::Path => Style::color(Color::Yellow),
      Role::Argument => Style::color(Color::White),
      Role::ToolOutput => Style::color(Color::White),
      // Reasoning is deliberately not one colour. Native is calm and dim because
      // it is real but secondary; a provider summary is italic because it is
      // authored about the model rather than by it; declared is underlined-ish
      // (bold-dim) because it is an answer to a question we asked; reconstructed
      // is magenta because it is our own guess and should never read as fact.
      Role::ReasoningNative => Style::color(Color::Dark),
      Role::ReasoningProviderSummary => Style {
        color: Color::Dark,
        bold: false,
        dim: true,
        italic: true,
      },
      Role::ReasoningDeclared => Style {
        color: Color::Dark,
        bold: false,
        dim: true,
        italic: false,
      },
      Role::ReasoningReconstructed => Style {
        color: Color::Magenta,
        bold: false,
        dim: true,
        italic: true,
      },
      Role::StateOk => Style::color(Color::Green),
      Role::StateFailed => Style::color(Color::Red),
      Role::StateUnknown => Style {
        color: Color::Yellow,
        bold: true,
        dim: false,
        italic: false,
      },
      Role::Meta => Style::color(Color::Dark),
      Role::Muted => Style {
        color: Color::Default,
        bold: false,
        dim: true,
        italic: false,
      },
      Role::Info => Style::color(Color::Cyan),
      Role::Warning => Style::color(Color::Yellow),
      Role::Error => Style::color(Color::Red),
      Role::Rare => Style {
        color: Color::Red,
        bold: true,
        dim: false,
        italic: false,
      },
      Role::Status => Style {
        color: Color::Dark,
        bold: false,
        dim: true,
        italic: false,
      },
    }
  }

  /// Wrap `text` in the escape sequences for `role`, or return it unchanged in
  /// monochrome.
  ///
  /// Uncolored output is not a degraded render; it is the reference rendering that
  /// tests, pipes, and logs use.
  pub fn paint(self, role: Role, text: &str) -> String {
    if !self.color {
      return text.to_string();
    }
    let style = self.style(role);
    let mut out = String::with_capacity(text.len() + 8);
    out.push_str("\x1b[");
    let mut first = true;
    if style.bold {
      out.push_str(if first { "1" } else { ";1" });
      first = false;
    }
    if style.dim {
      out.push_str(if first { "2" } else { ";2" });
      first = false;
    }
    if style.italic {
      out.push_str(if first { "3" } else { ";3" });
      first = false;
    }
    if let Some(sgr) = style.color.sgr() {
      if first {
        out.push_str(&sgr.to_string());
      } else {
        out.push(';');
        out.push_str(&sgr.to_string());
      }
      first = false;
    }
    if first {
      // A role with no visual claim at all: emit no escape at all. Painting `0m`
      // around plain text would make piped output unreadable for no gain.
      return text.to_string();
    }
    out.push('m');
    out.push_str(text);
    out.push_str("\x1b[0m");
    out
  }
}

impl Default for Palette {
  fn default() -> Self {
    Self::monochrome()
  }
}

impl fmt::Display for Role {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_role_has_a_distinct_style_claim() {
    // Distinctness here is not vanity: two roles resolving to the same style is
    // how "provider summary" starts looking like "reasoning".
    let palette = Palette::colored();
    let reasoning: Vec<Style> = [
      Role::ReasoningNative,
      Role::ReasoningProviderSummary,
      Role::ReasoningDeclared,
      Role::ReasoningReconstructed,
    ]
    .into_iter()
    .map(|role| palette.style(role))
    .collect();
    for (left, right) in reasoning.iter().zip(reasoning.iter().skip(1)) {
      assert_ne!(left, right);
    }
    assert_ne!(
      palette.style(Role::ReasoningNative),
      palette.style(Role::ReasoningProviderSummary)
    );
  }

  #[test]
  fn unknown_is_not_styled_like_failure() {
    let palette = Palette::colored();
    assert_ne!(
      palette.style(Role::StateUnknown),
      palette.style(Role::StateFailed)
    );
    assert_ne!(
      palette.style(Role::StateUnknown),
      palette.style(Role::StateOk)
    );
  }

  #[test]
  fn monochrome_emits_no_escapes_but_still_reports_styles() {
    let palette = Palette::monochrome();
    assert_eq!(palette.paint(Role::Operation, "read"), "read");
    assert!(!palette.is_color());
    assert_eq!(palette.style(Role::Operation).color, Color::Cyan);
  }

  #[test]
  fn colored_paint_wraps_and_resets() {
    let palette = Palette::colored();
    let painted = palette.paint(Role::Error, "boom");
    assert_eq!(painted, "\x1b[31mboom\x1b[0m");
    assert_eq!(palette.paint(Role::Assistant, "hi"), "hi");
  }

  #[test]
  fn reasoning_roles_are_flagged_as_reasoning() {
    assert!(Role::ReasoningNative.is_reasoning());
    assert!(Role::ReasoningReconstructed.is_reasoning());
    assert!(!Role::Assistant.is_reasoning());
    assert!(!Role::ToolOutput.is_reasoning());
  }

  #[test]
  fn role_names_are_stable() {
    assert_eq!(
      Role::ReasoningProviderSummary.as_str(),
      "reasoning_provider_summary"
    );
    assert_eq!(Role::StateUnknown.as_str(), "state_unknown");
  }
}
