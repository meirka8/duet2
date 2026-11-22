// SPDX-License-Identifier: MIT
//! `docs/crash-safety.md` and the matrix must name the same tests.
//!
//! The document is the contract ("T-10.2.1's AC is ... a checklist of
//! exactly these named tests -- not an open-ended fuzz target"). A row
//! added to the table without a test, or a test renamed away from its
//! row, would let the release gate pass while covering less than it
//! claims; this test makes either a failure.

use std::collections::BTreeSet;

const DOCUMENT: &str = include_str!("../../../../docs/crash-safety.md");
const MATRIX: &str = include_str!("sigkill_matrix.rs");

/// Every backticked test name in a table row's last column.
fn documented() -> BTreeSet<String> {
    DOCUMENT
        .lines()
        .filter(|line| line.starts_with('|'))
        .filter_map(|line| line.trim_end().trim_end_matches('|').rsplit('|').next())
        .flat_map(|cell| cell.split('`').skip(1).step_by(2))
        .filter(|name| name.contains("_sigkill_"))
        // A full test name, not an abbreviated mention of one elsewhere in
        // the document ("`copyfile_sigkill_mid_write_...`").
        .filter(|name| {
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// Every `#[test] fn` in the matrix.
fn implemented() -> BTreeSet<String> {
    let lines: Vec<&str> = MATRIX.lines().collect();
    lines
        .windows(2)
        .filter(|pair| pair[0].trim() == "#[test]")
        .filter_map(|pair| pair[1].trim().strip_prefix("fn "))
        .filter_map(|rest| rest.split('(').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_row_of_the_crash_safety_table_has_its_test_and_every_test_its_row() {
    let documented = documented();
    let implemented = implemented();
    assert!(
        documented.len() >= 45,
        "the table parser found only {} rows -- has docs/crash-safety.md changed shape?",
        documented.len()
    );
    let missing: Vec<_> = documented.difference(&implemented).collect();
    let undocumented: Vec<_> = implemented.difference(&documented).collect();
    assert!(
        missing.is_empty() && undocumented.is_empty(),
        "docs/crash-safety.md and tests/data_safety/sigkill_matrix.rs disagree.\n\
         rows with no test: {missing:#?}\ntests with no row: {undocumented:#?}"
    );
}

/// The table's own coverage summary must add up to its rows.
#[test]
fn the_crash_safety_table_summary_matches_its_rows() {
    let total = DOCUMENT
        .lines()
        .find(|line| line.contains("**Total rows**"))
        .and_then(|line| {
            line.split("**")
                .filter_map(|cell| cell.trim().parse::<usize>().ok())
                .next()
        })
        .expect("the summary table states a total");
    assert_eq!(total, documented().len());
}
