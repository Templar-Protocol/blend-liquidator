//! Access to the committed mainnet snapshot for unit tests.
//!
//! The file is embedded at compile time so tests need no working-directory
//! assumptions. See `tests/fixtures/README.md` for the capture procedure.

use serde_json::Value;

/// The raw fixture text.
pub(crate) const MAINNET_FIXED_V2: &str = include_str!("../tests/fixtures/mainnet-fixed-v2.json");

/// Parses the fixture; tests may `expect` because a malformed fixture is a
/// test-suite bug, not a runtime condition.
pub(crate) fn mainnet_fixed_v2() -> Value {
    serde_json::from_str(MAINNET_FIXED_V2).expect("fixture JSON parses")
}

/// A string member at `path` (e.g. `&["reserves", "0", "asset"]`), where a
/// numeric segment indexes an array.
pub(crate) fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for segment in path {
        current = match segment.parse::<usize>() {
            Ok(index) => &current[index],
            Err(_) => &current[*segment],
        };
    }
    current
        .as_str()
        .unwrap_or_else(|| panic!("fixture path {path:?} is not a string"))
}

#[cfg(test)]
mod tests {
    use super::{mainnet_fixed_v2, text};

    #[test]
    fn parses_and_matches_known_fields() {
        let fixture = mainnet_fixed_v2();

        assert_eq!(fixture["ledger"], 64_271_347);
        assert_eq!(fixture["ledger_close_time"], 1_788_534_414);
        assert_eq!(
            text(&fixture, &["pool"]),
            "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD"
        );
    }
}
