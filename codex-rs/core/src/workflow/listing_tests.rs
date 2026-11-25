//! Tests for workflow listing.

use super::*;

fn entry(name: &str, scope: WorkflowScope, description: &str) -> ListingEntry {
    ListingEntry {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use: None,
        scope,
        source: None,
    }
}

#[test]
fn resolution_priority_is_bundled_builtin_project_user() {
    let mut entries = vec![
        entry("a", WorkflowScope::User, "user"),
        entry("b", WorkflowScope::Project, "project"),
        entry("c", WorkflowScope::Builtin, "builtin"),
        entry("d", WorkflowScope::Bundled, "bundled"),
    ];
    sort_entries(&mut entries);
    let scopes: Vec<_> = entries.iter().map(|entry| entry.scope).collect();
    assert_eq!(
        scopes,
        vec![
            WorkflowScope::Bundled,
            WorkflowScope::Builtin,
            WorkflowScope::Project,
            WorkflowScope::User
        ]
    );
}

#[test]
fn same_priority_sorts_by_name() {
    let mut entries = vec![
        entry("zeta", WorkflowScope::User, "z"),
        entry("alpha", WorkflowScope::User, "a"),
    ];
    sort_entries(&mut entries);
    assert_eq!(entries[0].name, "alpha");
}

#[test]
fn shadowed_entries_are_marked_after_the_winner() {
    let mut entries = vec![
        entry("probe", WorkflowScope::User, "user probe"),
        entry("probe", WorkflowScope::Bundled, "bundled probe"),
    ];
    let rendered = render(&mut entries);
    assert!(rendered.contains("probe [bundled]: bundled probe"));
    assert!(rendered.contains("probe [user] (shadowed): user probe"));
}

#[test]
fn summaries_are_truncated_to_the_byte_budget() {
    let mut long = entry("long", WorkflowScope::User, "");
    long.description = "x".repeat(MAX_LISTING_SUMMARY_BYTES);
    assert_eq!(long.summary().len(), MAX_LISTING_SUMMARY_BYTES);
    long.description = "y".repeat(MAX_LISTING_SUMMARY_BYTES + 10);
    let summary = long.summary();
    assert!(summary.len() <= MAX_LISTING_SUMMARY_BYTES);
    assert!(summary.ends_with('…'));
}

#[test]
fn truncation_never_splits_a_utf8_character() {
    let value = "工作流".repeat(100);
    let summary = truncate_utf8(&value, 10);
    assert!(summary.len() <= 10);
    assert!(summary.is_char_boundary(summary.len()));
    assert!(summary.ends_with('…'));
}

#[test]
fn description_and_when_to_use_are_joined_once() {
    let mut item = entry("probe", WorkflowScope::Builtin, "does work");
    item.when_to_use = Some("use it when needed".into());
    assert_eq!(item.summary(), "does work — use it when needed");
    item.when_to_use = Some("   ".into());
    assert_eq!(item.summary(), "does work");
}

#[test]
fn resolution_comparator_matches_sort_order() {
    let bundled = entry("x", WorkflowScope::Bundled, "");
    let user = entry("x", WorkflowScope::User, "");
    assert_eq!(compare_for_resolution(&bundled, &user), Ordering::Less);
    assert_eq!(compare_for_resolution(&user, &bundled), Ordering::Greater);
}
