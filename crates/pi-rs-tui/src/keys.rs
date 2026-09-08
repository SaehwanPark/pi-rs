//! Terminal events to editor intents.
//!
//! One function, so the interactive loop has exactly one decision to make about
//! input: is this event for the buffer, or is it for me? Keeping that decision in
//! a table instead of inline is what lets the bindings be tested without a
//! terminal, and lets a later keymap override replace one module rather than
//! audit every call site.
//!
//! # Bound
//!
//! | key | intent |
//! | --- | --- |
//! | character, unmodified | `Insert` |
//! | `Enter` | `Submit` |
//! | `Esc` | `Cancel` |
//! | `Backspace`, `Ctrl-H` | `Backspace` |
//! | `Delete`, `Ctrl-D` | `DeleteForward` |
//! | `Left`, `Ctrl-B` | `MoveLeft` |
//! | `Right`, `Ctrl-F` | `MoveRight` |
//! | `Alt-B`, `Alt-F` | word motion |
//! | `Up`, `Down` | row motion, or recall at an edge |
//! | `Home`, `Ctrl-A` | line start, then indent, then end |
//! | `End`, `Ctrl-E` | line end |
//! | `Ctrl-K`, `Ctrl-U` | delete to line end, or to line start |
//! | `Ctrl-W` | delete word backward |
//! | `Ctrl-J`, `Ctrl-M` | insert a newline |
//! | bracketed paste | `Paste` |
//!
//! Motion bindings are the readline set, because that is the set muscle memory
//! from every other terminal brings, and a half-covered subset of it is worse than
//! none: a user who has learned `Ctrl-A` for line start should not have to learn
//! which of its neighbours this program also implemented.
//!
//! # Deliberately not bound
//!
//! **`Ctrl-C` is not [`Intent::Cancel`].** Interrupting a running turn and
//! discarding half-typed text are different actions that happen to share a key,
//! and only the loop knows whether a turn is running. The buffer's `Cancel` stays
//! reachable on `Esc`, and the loop keeps `Ctrl-C` for itself.
//!
//! `Tab` (completion), `Ctrl-L` (repaint), the function keys, and `Alt` on
//! terminals that deliver it as `Esc` plus a character are the same story: they
//! belong to a surface, or to a later keymap, and a mapping that silently ate them
//! would look like a dead key.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::editor::Intent;

/// What the buffer should make of one terminal event.
///
/// Anything that is not a key press or a paste is [`Intent::Noop`], including a
/// key release: on platforms that report both, treating a release as a second
/// keystroke types every character twice.
pub fn intent(event: &Event) -> Intent {
  match event {
    Event::Paste(text) => Intent::Paste(text.clone()),
    Event::Key(key) => key_intent(key),
    _ => Intent::Noop,
  }
}

/// What the buffer should make of one key event.
pub fn key_intent(key: &KeyEvent) -> Intent {
  if key.kind == KeyEventKind::Release {
    return Intent::Noop;
  }
  let control = key.modifiers.contains(KeyModifiers::CONTROL);
  let alt = key.modifiers.contains(KeyModifiers::ALT);
  let KeyCode::Char(raw) = key.code else {
    return match key.code {
      KeyCode::Backspace => Intent::Backspace,
      KeyCode::Delete => Intent::DeleteForward,
      KeyCode::Enter => Intent::Submit,
      KeyCode::Esc => Intent::Cancel,
      KeyCode::Up => Intent::MoveUp,
      KeyCode::Down => Intent::MoveDown,
      KeyCode::Left => Intent::MoveLeft,
      KeyCode::Right => Intent::MoveRight,
      KeyCode::Home => Intent::MoveLineStart,
      KeyCode::End => Intent::MoveLineEnd,
      _ => Intent::Noop,
    };
  };
  if control {
    return match lower(raw) {
      'a' => Intent::MoveLineStart,
      'b' => Intent::MoveLeft,
      'd' => Intent::DeleteForward,
      'e' => Intent::MoveLineEnd,
      'f' => Intent::MoveRight,
      'h' => Intent::Backspace,
      'j' | 'm' | '\n' | '\r' => Intent::InsertNewline,
      'k' => Intent::DeleteToLineEnd,
      'u' => Intent::DeleteToLineStart,
      'w' => Intent::DeleteWordBackward,
      _ => Intent::Noop,
    };
  }
  if alt {
    return match lower(raw) {
      'b' => Intent::MoveWordLeft,
      'f' => Intent::MoveWordRight,
      _ => Intent::Noop,
    };
  }
  // A terminal that reports Enter as a bare control character would otherwise
  // have that character swallowed by the buffer. Inserting a newline instead is
  // visible, and visible is recoverable.
  if raw == '\n' || raw == '\r' {
    return Intent::InsertNewline;
  }
  Intent::Insert(raw)
}

/// Case-folded binding lookup. Bindings are ASCII, so a non-ASCII character
/// reaches the buffer untouched.
fn lower(c: char) -> char {
  if c.is_ascii() {
    c.to_ascii_lowercase()
  } else {
    c
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
  }

  fn ctrl(c: char) -> Event {
    key(KeyCode::Char(c), KeyModifiers::CONTROL)
  }

  fn alt(c: char) -> Event {
    key(KeyCode::Char(c), KeyModifiers::ALT)
  }

  #[test]
  fn unmodified_characters_are_typeable() {
    assert_eq!(intent(&key(KeyCode::Char('x'), KeyModifiers::NONE)), {
      Intent::Insert('x')
    });
    assert_eq!(intent(&key(KeyCode::Char('한'), KeyModifiers::NONE)), {
      Intent::Insert('한')
    });
    // Shift is how a capital letter arrives, not a separate binding.
    assert_eq!(
      intent(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
      Intent::Insert('A')
    );
  }

  #[test]
  fn only_enter_submits_and_only_escape_cancels() {
    assert_eq!(
      intent(&key(KeyCode::Enter, KeyModifiers::NONE)),
      Intent::Submit
    );
    assert_eq!(
      intent(&key(KeyCode::Esc, KeyModifiers::NONE)),
      Intent::Cancel
    );
    // Nothing else in the space of ordinary keys may produce either action: an
    // accidental submission sends a prompt, and an accidental cancel eats a draft.
    let codes = [
      KeyCode::Backspace,
      KeyCode::Delete,
      KeyCode::Up,
      KeyCode::Down,
      KeyCode::Left,
      KeyCode::Right,
      KeyCode::Home,
      KeyCode::End,
      KeyCode::Tab,
      KeyCode::BackTab,
      KeyCode::PageUp,
      KeyCode::PageDown,
      KeyCode::Insert,
      KeyCode::F(3),
      KeyCode::Char('\u{0}'),
    ];
    for code in codes {
      for modifiers in [
        KeyModifiers::NONE,
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
      ] {
        let got = intent(&key(code, modifiers));
        assert!(
          got != Intent::Submit && got != Intent::Cancel,
          "{code:?} with {modifiers:?} produced {got:?}"
        );
      }
    }
  }

  #[test]
  fn emacs_bindings_cover_the_motion_set() {
    let cases = [
      (ctrl('a'), Intent::MoveLineStart),
      (ctrl('b'), Intent::MoveLeft),
      (ctrl('d'), Intent::DeleteForward),
      (ctrl('e'), Intent::MoveLineEnd),
      (ctrl('f'), Intent::MoveRight),
      (ctrl('h'), Intent::Backspace),
      (ctrl('j'), Intent::InsertNewline),
      (ctrl('k'), Intent::DeleteToLineEnd),
      (ctrl('u'), Intent::DeleteToLineStart),
      (ctrl('w'), Intent::DeleteWordBackward),
      (ctrl('A'), Intent::MoveLineStart),
      (alt('b'), Intent::MoveWordLeft),
      (alt('f'), Intent::MoveWordRight),
    ];
    for (event, expected) in cases {
      assert_eq!(intent(&event), expected, "{event:?}");
    }
  }

  #[test]
  fn arrow_keys_and_their_control_equivalents_agree() {
    assert_eq!(intent(&key(KeyCode::Left, KeyModifiers::NONE)), {
      intent(&ctrl('b'))
    });
    assert_eq!(intent(&key(KeyCode::Right, KeyModifiers::NONE)), {
      intent(&ctrl('f'))
    });
    assert_eq!(intent(&key(KeyCode::Home, KeyModifiers::NONE)), {
      intent(&ctrl('a'))
    });
    assert_eq!(intent(&key(KeyCode::End, KeyModifiers::NONE)), {
      intent(&ctrl('e'))
    });
  }

  #[test]
  fn ctrl_c_is_not_the_buffers_to_answer() {
    // Interrupting a turn and discarding text are different actions that share a
    // key, and only the loop knows which one is happening.
    assert_eq!(intent(&ctrl('c')), Intent::Noop);
    assert_eq!(intent(&ctrl('l')), Intent::Noop);
    assert_eq!(intent(&ctrl('z')), Intent::Noop);
    assert_eq!(intent(&key(KeyCode::Tab, KeyModifiers::NONE)), Intent::Noop);
  }

  #[test]
  fn a_released_key_is_not_a_second_keystroke() {
    let mut press = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    press.kind = KeyEventKind::Press;
    assert_eq!(key_intent(&press), Intent::Insert('x'));
    let mut release = press;
    release.kind = KeyEventKind::Release;
    assert_eq!(key_intent(&release), Intent::Noop);
  }

  #[test]
  fn a_paste_arrives_untouched() {
    let text = "fn main() {\n\tlet x = 1;\n}\n".to_string();
    assert_eq!(intent(&Event::Paste(text.clone())), Intent::Paste(text));
  }

  #[test]
  fn unrelated_events_are_not_intents() {
    assert_eq!(intent(&Event::Resize(80, 24)), Intent::Noop);
    assert_eq!(
      intent(&Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 3,
        row: 1,
        modifiers: KeyModifiers::NONE,
      })),
      Intent::Noop
    );
  }

  #[test]
  fn a_terminal_that_sends_a_bare_newline_moves_the_caret() {
    // Swallowing the character would look like a dead Enter; inserting a newline
    // is wrong in a way the user can see and undo.
    assert_eq!(
      intent(&key(KeyCode::Char('\r'), KeyModifiers::NONE)),
      Intent::InsertNewline
    );
    assert_eq!(
      intent(&key(KeyCode::Char('\n'), KeyModifiers::NONE)),
      Intent::InsertNewline
    );
  }

  #[test]
  fn no_two_control_bindings_share_a_key() {
    // A collision here is how one binding silently shadows another, and control
    // characters are exactly the keys a user cannot inspect.
    let letters = "abcdefghijklmnopqrstuvwxyz";
    let mut seen: Vec<(char, Intent)> = Vec::new();
    for c in letters.chars() {
      let intent = intent(&ctrl(c));
      if intent == Intent::Noop {
        continue;
      }
      assert!(seen.iter().all(|(other, _)| *other != c), "{c} bound twice");
      seen.push((c, intent));
    }
    assert_eq!(seen.len(), 11, "the control table silently lost a binding");
  }
}
