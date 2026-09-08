//! The candidate list a surface completes against, and the pure step rule Tab follows.
//!
//! Completion is kept out of the editor for the same reason the command parser is:
//! which names exist is the surface's knowledge (it knows about `/help` and `/quit`;
//! the buffer knows about carets), and the cycling is arithmetic over a list that
//! wants no terminal at all to test. The editor owns only the press counter and the
//! splice; everything that decides *what text appears* is here.
//!
//! # The Tab rule
//!
//! A word is the slash-prefixed name being completed, `/he` say. Pressing Tab:
//!
//! 1. the first press extends the word to the longest prefix the candidates share;
//!    when exactly one candidate matches, that is the name itself, and the trailing
//!    space says so;
//! 2. each further press replaces the word with the next candidate in list order,
//!    wrapping around;
//! 3. a press that would change nothing changes nothing — but it still advances the
//!    cycling, so the next press on an unextendable word starts walking the list.
//!
//! Case is significant: the names a surface registers are drawn verbatim, and a
//! completion that quietly folded case could offer a name the parser would not
//! accept.

/// One candidate list, in the order Tab walks it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completions {
  words: Vec<String>,
}

impl Completions {
  /// A list from any names, with or without the leading `/`. Empty names are
  /// dropped; duplicates collapse to their first appearance, because Tab walking
  /// the same name twice is a stuck key, not a choice.
  pub fn new<T: AsRef<str>, I: IntoIterator<Item = T>>(words: I) -> Self {
    let mut seen: Vec<String> = Vec::new();
    for word in words {
      let word = word.as_ref().trim().trim_start_matches('/').to_string();
      if !word.is_empty() && !seen.contains(&word) {
        seen.push(word);
      }
    }
    Self { words: seen }
  }

  /// The registered names, without slashes, in list order.
  pub fn words(&self) -> &[String] {
    &self.words
  }

  /// `true` when there is nothing to complete to, which makes Tab a noop wherever
  /// it lands.
  pub fn is_empty(&self) -> bool {
    self.words.is_empty()
  }

  /// The names that start with `after_slash`, in list order.
  pub fn matching(&self, after_slash: &str) -> Vec<&str> {
    self
      .words
      .iter()
      .map(String::as_str)
      .filter(|name| name.starts_with(after_slash))
      .collect()
  }

  /// What `word` (the slash-prefixed text, `/he`) becomes on the `press`-th Tab,
  /// counting from zero, or `None` when nothing matches and Tab should stay a noop.
  ///
  /// Press zero is the shared prefix of every match — the whole name plus a
  /// trailing space when there is exactly one. Later presses walk the matches in
  /// list order and wrap.
  pub fn step(&self, word: &str, press: usize) -> Option<String> {
    let after_slash = word.strip_prefix('/')?;
    let candidates = self.matching(after_slash);
    if candidates.is_empty() {
      return None;
    }
    if candidates.len() == 1 {
      // One name can only mean itself; the space ends the word so the next key is
      // already an argument, and no further press has anything left to offer.
      return (press == 0).then(|| format!("/{} ", candidates[0]));
    }
    if press == 0 {
      let prefix = '/'.to_string() + &common_prefix(&candidates);
      return Some(prefix);
    }
    let index = (press - 1) % candidates.len();
    Some(format!("/{}", candidates[index]))
  }
}

/// The longest prefix shared by every name in `names`.
fn common_prefix(names: &[&str]) -> String {
  let mut prefix = names[0].to_string();
  for name in &names[1..] {
    while !name.starts_with(&prefix) && !prefix.is_empty() {
      prefix.pop();
    }
    if prefix.is_empty() {
      break;
    }
  }
  prefix
}

#[cfg(test)]
mod tests {
  use super::*;

  fn list() -> Completions {
    Completions::new(["help", "quit", "exit"])
  }

  #[test]
  fn names_are_stored_slashless_deduplicated_and_in_order() {
    let completions = Completions::new(["/help", "help", "", "/quit"]);
    assert_eq!(completions.words(), ["help", "quit"]);
    assert!(!completions.is_empty());
    assert!(Completions::new(Vec::<String>::new()).is_empty());
  }

  #[test]
  fn matching_keeps_case_and_list_order() {
    let completions = Completions::new(["Quit", "quit"]);
    assert_eq!(completions.matching("qui"), ["quit"]);
    assert_eq!(completions.matching(""), ["Quit", "quit"]);
  }

  #[test]
  fn the_first_press_shares_what_the_candidates_have() {
    // `/e` has one match, so it is answered outright, with the word closed off.
    assert_eq!(list().step("/e", 0).as_deref(), Some("/exit "));
    // `/` matches three names sharing nothing, so the word cannot grow, but the
    // caller still counts the press and the next one walks the list.
    assert_eq!(list().step("/", 0).as_deref(), Some("/"));
    // A shared stem is the whole story of the first press.
    let both = Completions::new(["compact", "compare", "quit"]);
    assert_eq!(both.step("/c", 0).as_deref(), Some("/compa"));
  }

  #[test]
  fn later_presses_walk_the_list_and_wrap() {
    let found = |press| list().step("/", press);
    assert_eq!(found(1).as_deref(), Some("/help"));
    assert_eq!(found(2).as_deref(), Some("/quit"));
    assert_eq!(found(3).as_deref(), Some("/exit"));
    assert_eq!(found(4).as_deref(), Some("/help"));
  }

  #[test]
  fn cycling_offers_nothing_the_word_cannot_extend() {
    // No candidate starts with this, so Tab must not rewrite the word.
    assert_eq!(list().step("/nope", 0), None);
    assert_eq!(list().step("/nope", 3), None);
    // Not a slash word at all: not this rule's business.
    assert_eq!(list().step("help", 0), None);
  }

  #[test]
  fn a_single_candidate_stops_offering_after_taking() {
    // Once the word *is* the name, a repeated press must not keep appending spaces.
    assert_eq!(list().step("/exit", 0).as_deref(), Some("/exit "));
    assert_eq!(list().step("/exit", 1), None);
  }

  #[test]
  fn common_prefix_of_disjoint_names_is_empty() {
    assert_eq!(common_prefix(&["abc", "xyz"]), "");
    assert_eq!(common_prefix(&["abc", "abd"]), "ab");
  }
}
