//! `pi-rs interactive`: one durable session, many turns, one terminal.
//!
//! The buffer ([`pi_rs_tui::editor`]), the keymap ([`pi_rs_tui::keys`]), and the
//! transcript renderer already exist. What they deliberately do not contain is the
//! loop: reading keys, owning raw mode, and deciding whether a keystroke means
//! "leave" are composition and terminal plumbing, and a crate that renders events
//! must not be the one that decides what a key means. That is why this module lives
//! in the composition root and pulls the runtime in through [`crate::run`].
//!
//! # One screen
//!
//! ```text
//! > what the user is typing           <- Editor::display(PROMPT_PREFIX)
//!   and its continuation rows
//! openai/gpt-5 · idle · enter submits, ctrl-c quits   <- one status line
//! ```
//!
//! There is no alternate screen. A turn's answer and transcript are ordinary writes
//! to stdout and stderr, and they belong in the terminal's scrollback the way any
//! other program's output does; a frame that kept its ordering intact would have to
//! own the whole screen and take the runtime's output with it. So the frame is
//! drawn, erased before each turn, and redrawn underneath whatever the turn printed.
//!
//! # Redraw discipline
//!
//! A frame is written only when something visible changed: the buffer changed, the
//! terminal was resized, or a turn ended. [`Outcome::Unchanged`] and an unmapped key
//! draw nothing, which is what keeps a held-down key from repainting the screen.
//!
//! # Why raw mode is suspended for a turn
//!
//! `crossterm::terminal::enable_raw_mode` is `cfmakeraw`, which clears `OPOST` and
//! `ONLCR`: a `\n` stops carrying the column reset with it. This module writes its
//! own `\r\n`, but a turn's output is written by the runtime through plain writers
//! that do not know they are attached to a raw terminal, and would render as a
//! staircase. Raw mode is therefore handed back for the duration of a turn. The
//! side effect is honest and documented: inside a turn, Ctrl-C is the terminal's own
//! signal rather than a key this loop reads, and interrupting a turn is a separate
//! runtime slice rather than something to imitate here.

use std::io::{self, Write};

use crossterm::{
  cursor::{MoveToColumn, MoveToPreviousLine},
  event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
  queue,
  terminal::{self, Clear, ClearType},
};

use pi_rs_core::TurnStatus;
use pi_rs_runtime::TurnError;
use pi_rs_tui::{Editor, Intent, Outcome, display_width, keys::intent, statusline, term, truncate};

use crate::{
  cli::{InteractiveArgs, SurfaceArgs},
  run::{self, SessionHandle},
};

/// What is drawn before the first row of the buffer.
const PROMPT_PREFIX: &str = "> ";

/// What the loop does with a turn that has ended.
///
/// A cancellation is something the user did on purpose, so it is not a reason to
/// lose the session: the loop says so in the transcript, where there is room to say
/// what happened, and takes the buffer back. Only a failure the loop cannot see a
/// way past ends it.
enum AfterTurn {
  /// The turn ran to the end; it counts.
  Done,
  /// What the loop prints about the user's own cancellation.
  Cancelled(&'static str),
  /// The failure that ends the session.
  Failed(run::SessionError),
}

/// Sort one turn's result into those three cases.
///
/// The status line is not what reports a cancellation. It cannot see why a turn
/// stopped, so a line that tried to say more than `waiting` would be guessing; it
/// goes back to waiting and the loop owns the explanation.
fn after_turn(result: Result<(), TurnError>) -> AfterTurn {
  match result {
    Ok(()) => AfterTurn::Done,
    Err(TurnError::Aborted(TurnStatus::Cancelled)) => AfterTurn::Cancelled("turn cancelled"),
    Err(error) => AfterTurn::Failed(run::SessionError::Turn(error)),
  }
}

/// Columns to assume when the terminal will not say how wide it is.
///
/// Reaching this needs a terminal that answers `is a terminal` but not `size`,
/// which is unusual enough that guessing is better than refusing to draw.
const FALLBACK_COLUMNS: usize = 80;

/// What the status line reports about the session's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
  /// Waiting for input.
  Idle,
  /// A turn is running; no event is read until it ends.
  Working,
}

/// What one terminal event asks the loop to do.
///
/// The whole of the loop's decision-making, made in a function that has never seen
/// a terminal: see [`action`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoopAction {
  /// Hand this intent to the buffer, then repaint if the buffer changed.
  Edit(Intent),
  /// Ctrl-C with something typed: keep the draft and do nothing else.
  KeepDraft,
  /// Leave the loop.
  Quit,
  /// The terminal is `columns` wide now; reflow the buffer and repaint.
  Resize { columns: usize },
}

/// Decide what one event means, without touching a terminal.
///
/// `Ctrl-C` is decided here rather than in the keymap, and the fact the keymap
/// cannot have is whether anything is typed, which `buffer_is_empty` supplies. An
/// empty buffer means the user meant to leave. Anything in it means they did not,
/// and neither exiting nor discarding their text is this loop's to choose.
///
/// Everything else goes to the keymap, including events it reports as
/// [`Intent::Noop`]: an unmapped key is an edit that changed nothing, and "changed
/// nothing" is already how the loop decides not to repaint.
pub fn action(event: &Event, buffer_is_empty: bool) -> LoopAction {
  if is_ctrl_c(event) {
    return if buffer_is_empty {
      LoopAction::Quit
    } else {
      LoopAction::KeepDraft
    };
  }
  if let Event::Resize(columns, _rows) = event {
    // Rows are what a surface that scrolls or pages needs. This one draws at the
    // bottom of the screen and never scrolls, so only the width changes anything.
    return LoopAction::Resize {
      columns: usize::from(*columns),
    };
  }
  LoopAction::Edit(intent(event))
}

/// The one key the keymap deliberately refuses to bind.
///
/// A terminal delivering `Ctrl-C` as the byte `0x03` still arrives here, because
/// crossterm decodes that byte to `Char('c')` with `CONTROL`. A release is ignored:
/// on platforms that report press and release, treating both as keystrokes would
/// double every edit, and a release cannot be the first statement of an intent.
fn is_ctrl_c(event: &Event) -> bool {
  let Event::Key(key) = event else {
    return false;
  };
  key.kind != KeyEventKind::Release
    && key.modifiers.contains(KeyModifiers::CONTROL)
    && matches!(key.code, KeyCode::Char('c' | 'C'))
}

/// What the waiting line offers the user.
const WAITING_HINT: &str = "enter submits, ctrl-c quits";

/// The one status line: which model answers, and what the session is doing.
///
/// The wording belongs to [`statusline`]: it is a projection that can be tested
/// without a terminal, and the loop only says what it honestly knows. The model is
/// named in both states rather than only in one: knowing which model answered is
/// what makes a surprising answer interpretable afterwards.
///
/// `columns` is the width the editor is laid out at, because two width notions in
/// one frame is a bug. A status line that wraps leaves the frame holding more lines
/// than the loop counted, and every later redraw would land on the wrong row; the
/// projection cuts to that budget by dropping whole segments rather than wrapping.
fn status_line(model: &str, state: TurnState, turns: usize, columns: usize) -> String {
  let waiting = matches!(state, TurnState::Idle);
  let line = statusline::line(&statusline::Status {
    model,
    activity: if waiting {
      statusline::Activity::Waiting
    } else {
      statusline::Activity::Running
    },
    turns,
    columns,
    // The hint is what the loop accepts right now. While a turn runs, enter does not
    // submit, so it goes away rather than offering a key that does nothing.
    hint: waiting.then_some(WAITING_HINT),
  });
  // The projection's floor is one word, so it always says something; the loop's
  // budget is the harder rule, because a line of `columns + 1` is a redraw on the
  // wrong row. Where the projection could not drop its way into budget, the tail is
  // cut here, and with no columns at all that leaves nothing to draw.
  let plain = line.plain();
  if line.width() > columns {
    truncate(&plain, columns)
  } else {
    plain
  }
}

/// Rows of the frame for a buffer of `rows` display rows plus the status line.
fn frame_lines(rows: usize) -> usize {
  rows + 1
}

/// Lines to move up from just below the frame to land on display row `caret_row`.
///
/// The frame is `rows` buffer rows then the status line, and the writes above leave
/// the cursor one line below that, so the distance back is the whole frame minus the
/// row the caret is on.
fn caret_lines_up(rows: usize, caret_row: usize) -> usize {
  frame_lines(rows) - caret_row
}

/// A line count for a cursor move; terminals count these in 16 bits.
fn terminal_lines(lines: usize) -> u16 {
  u16::try_from(lines).unwrap_or(u16::MAX)
}

/// A column for a cursor move, on the same terms as [`terminal_lines`].
fn terminal_columns(columns: usize) -> u16 {
  u16::try_from(columns).unwrap_or(u16::MAX)
}

/// Raw mode for exactly as long as this value is alive.
///
/// `Drop` is the only exit, and that is the point: every path out of the loop —
/// Ctrl-C, a turn that failed, an early `?`, a panic unwinding through a caller —
/// passes through here, so the terminal is never left in a mode that nothing is
/// driving any more.
struct RawTerminal;

impl RawTerminal {
  fn enter() -> io::Result<Self> {
    terminal::enable_raw_mode()?;
    Ok(Self)
  }
}

impl Drop for RawTerminal {
  fn drop(&mut self) {
    // A destructor has nowhere to report a failure and nothing to retry it with.
    // Failing here means the terminal is still raw, which the caller cannot fix
    // either; the reason to have entered raw mode is gone, and this is the last
    // thing this program can do about it.
    let _ = terminal::disable_raw_mode();
  }
}

/// The interactive loop: the buffer, the frame it draws, the events it reads.
struct Loop {
  editor: Editor,
  /// What the status line names as the model.
  model: String,
  state: TurnState,
  /// Turns that have finished. The status line counts them; `0` says nothing yet.
  turns: usize,
  /// Columns available to this surface, as the terminal last reported them.
  columns: usize,
  /// How many lines the cursor sits below the row the current frame starts on.
  ///
  /// Not the frame's height: a frame ends with the cursor parked on the caret, and
  /// the caret is usually above the frame's bottom row. This is the distance an erase
  /// has to travel to get back to the top of what the loop owns, and `0` means the
  /// loop owns no rows — which is the state during and after handing the screen to a
  /// turn, because everything below that point belongs to the turn's output.
  above: usize,
}

impl Loop {
  fn new(model: String, columns: usize) -> Self {
    let mut surface = Self {
      editor: Editor::new(),
      model,
      state: TurnState::Idle,
      turns: 0,
      columns: 1,
      above: 0,
    };
    surface.set_columns(columns);
    surface
  }

  /// Set the width the buffer soft-wraps at and the status line is cut to.
  fn set_columns(&mut self, columns: usize) {
    self.columns = columns.max(1);
    // The prefix is drawn in front of the first row, so it spends columns that the
    // buffer cannot also spend on text.
    let text = self.columns.saturating_sub(display_width(PROMPT_PREFIX));
    self.editor.set_width(text.max(1));
  }

  /// The status line this surface would draw right now.
  ///
  /// Cut to [`Loop::columns`] — the same width the editor is laid out at — and
  /// never more than one line, which is what the frame counted.
  fn status(&self) -> String {
    status_line(&self.model, self.state, self.turns, self.columns)
  }

  /// Read events until the user leaves.
  ///
  /// A turn failure ends the loop with that failure, so the terminal is restored on
  /// the way out and the reason is what gets reported.
  fn run(&mut self, session: &mut SessionHandle<'_>) -> Result<(), String> {
    self.draw().map_err(terminal_failure)?;
    loop {
      let event = event::read().map_err(|error| format!("cannot read the terminal: {error}"))?;
      match action(&event, self.editor.is_empty()) {
        LoopAction::Quit => return Ok(()),
        // Something is typed, so the key meant "stop that", not "leave". The draft
        // stays where it is, and there is nothing new to draw.
        LoopAction::KeepDraft => {}
        LoopAction::Resize { columns } => {
          self.set_columns(columns);
          self.draw().map_err(terminal_failure)?;
        }
        LoopAction::Edit(key) => match self.editor.apply(key) {
          // Nothing moved, so nothing moved on the screen.
          Outcome::Unchanged => {}
          Outcome::Changed | Outcome::Cancelled => self.draw().map_err(terminal_failure)?,
          Outcome::Submit(prompt) => self.turn(session, &prompt)?,
        },
      }
    }
  }

  /// One turn of the open session, with the screen handed over while it runs.
  fn turn(&mut self, session: &mut SessionHandle<'_>, prompt: &str) -> Result<(), String> {
    self.hand_over().map_err(terminal_failure)?;
    // See the module comment: a turn writes plain `\n`s, and raw mode has taken the
    // terminal's own translation of them away. From here until the matching enable,
    // the terminal is the one the runtime's writers expect.
    terminal::disable_raw_mode().map_err(terminal_failure)?;
    let result = session.turn(prompt);
    // Failover changes which model answers, so the frame has to ask the runtime
    // rather than keep saying what the config said when the session opened.
    self.model = session.model().to_string();
    take_line().map_err(terminal_failure)?;
    let outcome = after_turn(result);
    // The loop's own line about a cancellation, written while the terminal still
    // translates it into a row of its own.
    if let AfterTurn::Cancelled(note) = &outcome {
      write_line(&mut io::stdout(), note).map_err(terminal_failure)?;
    }
    terminal::enable_raw_mode().map_err(terminal_failure)?;
    match outcome {
      AfterTurn::Done => self.turns += 1,
      // The note above is the report; the frame below it goes back to saying
      // `waiting`, which is all the projection is allowed to claim.
      AfterTurn::Cancelled(_) => {}
      AfterTurn::Failed(error) => return Err(run::session_error(error)),
    }
    self.state = TurnState::Idle;
    self.draw().map_err(terminal_failure)
  }

  /// Replace the frame with the one line that stays up while a turn runs.
  ///
  /// The buffer is gone because the submitted prompt is what the turn prints first,
  /// and a frame left on the screen would be counted as more lines than the terminal
  /// still holds by the time the next redraw lands.
  fn hand_over(&mut self) -> io::Result<()> {
    let mut out = io::stdout();
    self.erase(&mut out)?;
    self.state = TurnState::Working;
    write_line(&mut out, &self.status())?;
    out.flush()
  }

  /// Write the frame, then leave the terminal's cursor on the caret.
  fn draw(&mut self) -> io::Result<()> {
    let mut out = io::stdout();
    self.erase(&mut out)?;
    let layout = self.editor.display(PROMPT_PREFIX);
    for row in &layout.rows {
      write_line(&mut out, row)?;
    }
    write_line(&mut out, &self.status())?;
    let up = caret_lines_up(layout.rows.len(), layout.cursor.line);
    if up > 0 {
      queue!(out, MoveToPreviousLine(terminal_lines(up)))?;
    }
    queue!(out, MoveToColumn(terminal_columns(layout.cursor.column)))?;
    // Recorded after the move, because it says where the cursor ended up rather than
    // how much was written. The move leaves the cursor on the caret, and the caret's
    // line index is exactly its distance from the row the frame starts on.
    self.above = layout.cursor.line;
    out.flush()
  }

  /// Pull the cursor back to the row the last frame started on and erase from there.
  ///
  /// The distance is [`Loop::above`], not the frame's height: overshooting upward
  /// would take the erase into the output of an earlier turn, which is the record the
  /// user came here to read. Erasing downward is safe because a frame is always the
  /// most recent thing on the screen, so nothing below it belongs to anyone else.
  fn erase(&mut self, out: &mut impl Write) -> io::Result<()> {
    if self.above > 0 {
      queue!(out, MoveToPreviousLine(terminal_lines(self.above)))?;
      self.above = 0;
    }
    queue!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    out.flush()
  }
}

/// One line of the frame.
///
/// The column reset is written here rather than left to the terminal, because raw
/// mode has removed the translation that would otherwise have supplied it.
fn write_line(out: &mut impl Write, text: &str) -> io::Result<()> {
  out.write_all(text.as_bytes())?;
  out.write_all(b"\r\n")
}

/// Move onto a line of our own.
///
/// A streamed answer is not newline-terminated until the session closes, so after a
/// turn the cursor can be sitting at the end of one. Drawing the next frame without
/// taking a line would overwrite that answer, which is the one thing on the screen
/// the user asked for. Called while the terminal still translates a newline into a
/// row change, so this is the only place a bare `\n` is enough.
fn take_line() -> io::Result<()> {
  let mut out = io::stdout();
  out.write_all(b"\n")?;
  out.flush()
}

/// `pi-rs interactive`: a session that holds many turns.
pub fn execute(args: InteractiveArgs) -> Result<(), String> {
  if !term::Stream::Stdout.is_terminal() {
    // Checked before anything is opened, so a piped invocation is one clear line
    // rather than a terminal that nobody put back.
    return Err(
      "interactive needs a terminal on stdout; for one turn in a script use `pi-rs run`"
        .to_string(),
    );
  }
  // The surface is not configurable here. Colour and width come from the terminal
  // the transcript is written to, which this command already requires.
  let surface = SurfaceArgs::default();
  run::open_session(&args.config, &args.cwd, &surface, |session| {
    // Raw mode is entered only once the session is open, so a bad config stays what
    // it was: a line printed on a terminal nothing has rearranged. From here on the
    // guard is what restores it, including when `run` returns an error.
    let _terminal = RawTerminal::enter().map_err(terminal_failure)?;
    let columns = term::Stream::Stdout.width().unwrap_or(FALLBACK_COLUMNS);
    Loop::new(session.model().to_string(), columns).run(session)
  })
}

/// An I/O failure against the terminal, in the words this command reports.
fn terminal_failure(error: io::Error) -> String {
  format!("cannot use the terminal: {error}")
}

#[cfg(test)]
mod tests {
  use super::*;

  use crossterm::event::KeyEvent;

  fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))
  }

  fn ctrl_c(kind: KeyEventKind) -> Event {
    key(KeyCode::Char('c'), KeyModifiers::CONTROL, kind)
  }

  #[test]
  fn ctrl_c_on_an_empty_buffer_leaves_the_loop() {
    assert_eq!(action(&ctrl_c(KeyEventKind::Press), true), LoopAction::Quit);
  }

  #[test]
  fn ctrl_c_with_text_typed_neither_quits_nor_touches_the_buffer() {
    let action = action(&ctrl_c(KeyEventKind::Press), false);
    // Not `Quit`, and not an intent the buffer would be handed: `KeepDraft` is the
    // action whose only implementation is that the loop does nothing.
    assert_eq!(action, LoopAction::KeepDraft);
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("half a sentence".to_string()));
    if let LoopAction::Edit(intent) = action {
      editor.apply(intent);
    }
    assert_eq!(editor.text(), "half a sentence");
  }

  #[test]
  fn ctrl_c_is_caught_whatever_shape_the_terminal_sends() {
    // The byte `0x03` is decoded to `Char('c')` plus `CONTROL` by crossterm, and a
    // terminal that reports the shift state as well must not be a second case.
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Repeat), false),
      LoopAction::KeepDraft
    );
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Repeat), true),
      LoopAction::Quit
    );
    // A release is not a second statement of an intent.
    assert_eq!(
      action(&ctrl_c(KeyEventKind::Release), true),
      LoopAction::Edit(Intent::Noop)
    );
  }

  #[test]
  fn other_control_keys_reach_the_keymap() {
    // `Ctrl-C` is the only key taken away from the buffer, and taking it by
    // modifier alone would swallow `Ctrl-D`, `Ctrl-K`, and the rest of readline.
    assert_eq!(
      action(
        &key(
          KeyCode::Char('d'),
          KeyModifiers::CONTROL,
          KeyEventKind::Press
        ),
        true
      ),
      LoopAction::Edit(Intent::DeleteForward)
    );
    assert_eq!(
      action(
        &key(KeyCode::Char('c'), KeyModifiers::NONE, KeyEventKind::Press),
        true
      ),
      LoopAction::Edit(Intent::Insert('c'))
    );
  }

  #[test]
  fn everything_else_is_the_keymaps() {
    let enter = key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&enter, false), LoopAction::Edit(Intent::Submit));
    let escape = key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&escape, false), LoopAction::Edit(Intent::Cancel));
    // An event with no binding is an edit that changes nothing, which is already the
    // loop's signal not to repaint.
    let unknown = key(KeyCode::F(7), KeyModifiers::NONE, KeyEventKind::Press);
    assert_eq!(action(&unknown, true), LoopAction::Edit(Intent::Noop));
    let paste = Event::Paste("two\nlines".to_string());
    assert_eq!(
      action(&paste, true),
      LoopAction::Edit(Intent::Paste("two\nlines".to_string()))
    );
  }

  #[test]
  fn a_resize_reports_the_new_width() {
    let event = Event::Resize(120, 40);
    assert_eq!(action(&event, true), LoopAction::Resize { columns: 120 });
  }

  #[test]
  fn the_status_line_names_the_model_and_the_turn() {
    let idle = status_line("local/vulcan", TurnState::Idle, 0, 80);
    assert!(idle.starts_with("local/vulcan"));
    assert!(idle.contains("idle"));
    let working = status_line("local/vulcan", TurnState::Working, 1, 80);
    assert!(working.starts_with("local/vulcan"));
    assert!(working.contains("turn"));
    assert!(!working.contains("ctrl-c"));
    // One line, whatever the state.
    assert_eq!(idle.matches('\n').count(), 0);
    assert_eq!(working.matches('\n').count(), 0);
  }

  #[test]
  fn the_status_line_never_spills_onto_a_second_row() {
    let wide = status_line(
      "a/very-long-model-name-that-does-not-fit",
      TurnState::Idle,
      3,
      20,
    );
    assert!(display_width(&wide) <= 20, "{wide}");
    // No budget at all: the projection would still say `idle`, but nothing fits in
    // zero columns, and a drawn word there is the spill the frame cannot survive.
    assert_eq!(status_line("a/b", TurnState::Idle, 0, 0), "");
  }

  /// The line the loop draws is the projection's output, separators and all, so the
  /// wording cannot fork back into the loop the moment the projection changes.
  #[test]
  fn the_status_line_is_the_projection_word_for_word() {
    // `turns` is nonzero on purpose: a line that dropped the turn segment would
    // still match a hand-written `model · idle · hint`, so it proves nothing.
    let waiting = status_line("local/vulcan", TurnState::Idle, 2, 120);
    assert_eq!(
      waiting,
      statusline::line(&statusline::Status {
        model: "local/vulcan",
        activity: statusline::Activity::Waiting,
        turns: 2,
        columns: 120,
        hint: Some(WAITING_HINT),
      })
      .plain()
    );
    // The running line is the same projection with the other activity word and no
    // hint, because enter does not submit while a turn is in flight.
    let running = status_line("local/vulcan", TurnState::Working, 2, 120);
    assert_eq!(
      running,
      statusline::line(&statusline::Status {
        model: "local/vulcan",
        activity: statusline::Activity::Running,
        turns: 2,
        columns: 120,
        hint: None,
      })
      .plain()
    );
    assert!(!running.contains(WAITING_HINT));
  }

  #[test]
  fn a_frame_is_the_buffer_rows_plus_one_line() {
    // The empty buffer is one row, so the frame is that row and the status line.
    let editor = Editor::new();
    let layout = editor.display(PROMPT_PREFIX);
    assert_eq!(frame_lines(layout.rows.len()), 2);
    // Caret on the only row of a two-line frame: two lines up from below it.
    assert_eq!(caret_lines_up(1, 0), 2);
    // Caret on the last buffer row of a three-line frame: two lines up, so it never
    // lands on the status line or below it.
    assert_eq!(caret_lines_up(2, 1), 2);
    assert_eq!(terminal_lines(3), 3);
    assert_eq!(terminal_lines(usize::from(u16::MAX) + 5), u16::MAX);
    assert_eq!(terminal_columns(7), 7);
    assert_eq!(terminal_columns(usize::from(u16::MAX) + 5), u16::MAX);
  }

  #[test]
  fn an_erase_returns_to_the_row_the_frame_started_on() {
    // The cursor arithmetic a redraw depends on, as the terminal sees it: writing the
    // frame leaves the cursor one line below it, the move after that parks it on the
    // caret, and the erase has to end on the row the frame began on. A distance that
    // overshoots upward takes the erase into an earlier turn's output, which is the
    // one part of the screen the loop was never allowed to touch.
    for rows in 1..=4usize {
      for caret_row in 0..rows {
        let started_on = 40usize;
        let below_the_frame = started_on + frame_lines(rows);
        let parked_on_caret = below_the_frame - caret_lines_up(rows, caret_row);
        assert_eq!(parked_on_caret, started_on + caret_row);
        // What `Loop::erase` travels is the caret's own line index.
        assert_eq!(parked_on_caret - caret_row, started_on, "{rows} rows");
      }
    }
  }

  #[test]
  fn the_buffer_wraps_inside_the_terminal_minus_the_prefix() {
    let surface = Loop::new("local/vulcan".to_string(), 40);
    assert_eq!(surface.editor.width(), 38);
    // A width the buffer cannot work with still leaves it able to draw one column.
    let narrow = Loop::new("local/vulcan".to_string(), 1);
    assert_eq!(narrow.editor.width(), 1);
    assert_eq!(narrow.columns, 1);
  }
}
