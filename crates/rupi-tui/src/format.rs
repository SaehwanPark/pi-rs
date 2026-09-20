//! Small formatting helpers shared by every surface renderer.
//!
//! These live in one place because the surface repeats the same three claims
//! thousands of times: how big, how long, how many. Two renderers that format
//! durations differently in the same transcript read like two different tools
//! disagreeing.

/// The middle dot that separates facts inside one meta line.
pub const SEPARATOR: &str = " · ";

/// Byte counts the way a terminal user reads them.
///
/// Below 1 KiB the exact count matters — "how big is the diff" is a bytes
/// question — and above it a false precision of "1536 bytes" is noise.
pub fn format_bytes(bytes: u64) -> String {
  const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
  if bytes < 1024 {
    return format!("{bytes} B");
  }
  // One decimal below ten units, none above: "1.5 KiB" is a real difference and
  // "20.4 MiB" is not worth the extra character. The threshold is where the
  // decimal stops changing the reader's decision.
  let mut value = bytes as f64 / 1024.0;
  let mut unit = 1;
  while value >= 1024.0 && unit < UNITS.len() - 1 {
    value /= 1024.0;
    unit += 1;
  }
  if value >= 10.0 {
    format!("{:.0} {}", value, UNITS[unit])
  } else {
    format!("{value:.1} {}", UNITS[unit])
  }
}

/// Durations in the unit a human would say out loud.
pub fn format_duration_ms(ms: u64) -> String {
  if ms < 1000 {
    return format!("{ms} ms");
  }
  let seconds = ms / 1000;
  if seconds < 60 {
    let tenths = (ms % 1000) / 100;
    return format!("{seconds}.{tenths} s");
  }
  let minutes = seconds / 60;
  let rest = seconds % 60;
  if minutes < 60 {
    return format!("{minutes}m {rest:02}s");
  }
  let hours = minutes / 60;
  let rest_minutes = minutes % 60;
  format!("{hours}h {rest_minutes:02}m")
}

/// Token counts with thousands grouping, because context budgets are compared
/// by magnitude and `128000` vs `12800` is easy to misread at a glance.
pub fn format_tokens(tokens: u64) -> String {
  let digits = tokens.to_string();
  let mut out = String::with_capacity(digits.len() + digits.len() / 3);
  for (index, ch) in digits.chars().enumerate() {
    if index > 0 && (digits.len() - index) % 3 == 0 {
      out.push(',');
    }
    out.push(ch);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bytes_switch_units_at_kibibytes() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(2_097_152), "2.0 MiB");
    assert_eq!(format_bytes(20 * 1024 * 1024), "20 MiB");
  }

  #[test]
  fn durations_pick_a_spoken_unit() {
    assert_eq!(format_duration_ms(0), "0 ms");
    assert_eq!(format_duration_ms(999), "999 ms");
    assert_eq!(format_duration_ms(1000), "1.0 s");
    assert_eq!(format_duration_ms(3250), "3.2 s");
    assert_eq!(format_duration_ms(65_000), "1m 05s");
    assert_eq!(format_duration_ms(3_725_000), "1h 02m");
  }

  #[test]
  fn tokens_group_by_thousands() {
    assert_eq!(format_tokens(0), "0");
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1000), "1,000");
    assert_eq!(format_tokens(128_000), "128,000");
  }
}
