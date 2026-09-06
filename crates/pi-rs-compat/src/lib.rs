//! Reading what Pi left on disk, and calling anything unexpected an observation.
//!
//! Pi compatibility is behavioral and versioned (`COMPATIBILITY.md`), and the first
//! piece of it is files: skills, prompts, packages, and sessions that already exist
//! because a user installed Pi. The rules for reading them live here, one module per
//! format, each returning typed state plus the things it did not understand.
//!
//! Two rules run through the whole crate.
//!
//! **Silence is only for what is deliberately ignored.** When Pi itself skips a file
//! without comment, this crate skips it too; every other decision produces a warning
//! that names the path and the reason. An empty list that is really "we found nothing"
//! and an empty list that is really "we refused to look" must not look the same.
//!
//! **Compatibility output is observation, not pi-rs state.** A field in someone else's
//! file is evidence about that file. It does not become runtime state, an event, or a
//! capability unless something in the runtime deliberately adopts it.

pub mod frontmatter;
pub mod scan;
pub mod skill;
