//! The presentation layer for pi-rs surfaces.
//!
//! This crate answers one question — what does a canonical runtime event look
//! like on a terminal — and answers it without touching a socket, a file, or a
//! runtime. That boundary is the point: a renderer that can reach the runtime
//! eventually reaches the store, and then a display bug becomes a state bug.
//!
//! Four layers, each usable alone:
//!
//! * [`width`] — display-column measurement and wrapping. Terminals measure in
//!   columns, not bytes or chars; `String::len()` and `chars().count()` are both
//!   wrong for CJK, emoji, and combining marks.
//! * [`style`] — semantic [`Role`]s and a palette that resolves them. Renderers
//!   name meanings, never colours.
//! * [`mod@line`] — [`RenderLine`], a sequence of role-tagged segments. Plain text is
//!   the primary representation; ANSI is a projection of it.
//! * [`transcript`] — [`render_event`], the deterministic event-to-lines mapping
//!   shared by the live transcript, `pi-rs trace`, and any later export.
//!
//! Plus [`editor`] for the buffer a user types into, [`live`] for streaming,
//! [`command`] for input classification, [`mod@format`] for the repeated scalar
//! renderings, and [`term`] for the two environment probes a surface needs before
//! it can decide what to emit.
//!
//! # Two destinations
//!
//! Assistant prose goes to stdout raw; transcript chrome goes to stderr labelled.
//! A recorded session replays identically to a live run because both go through
//! [`render_event`].
//!
//! # What this crate is not
//!
//! Not a command registry, not an event bus, not an interactive loop. It
//! classifies input, holds text, and renders events; it never decides what a
//! command means or whether an action is allowed. Terminal plumbing — raw mode,
//! key decoding, redraw timing — belongs to whoever composes these pieces.

// The runtime vocabulary is imported by name, not globbed, so a new core variant
// is a compile error here instead of a silently unstyled line.

pub mod command;
pub mod editor;
pub mod format;
pub mod line;
pub mod live;
pub mod style;
pub mod term;
pub mod trace;
pub mod transcript;
pub mod width;

pub use command::{Input, Span};
pub use editor::{Cursor, Editor, Intent, Layout, Outcome};
pub use line::{NarrowDecoration, RenderLine, Segment};
pub use live::{Surface, is_streamed, routine_stream};
pub use style::{Color, Palette, Role, Style};
pub use term::{ColorChoice, Stream};
pub use trace::{RenderedEntry, TraceSelection, render_trace};
pub use transcript::{
  DiagnosticFilter, TranscriptOptions, completion_label, reasoning_label, reasoning_role,
  render_event, tool_request_line,
};
pub use width::{MIN_COLUMN, display_width, fits, truncate, wrap};
