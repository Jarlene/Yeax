//! Deterministic ordering and rendering for workflow listings.
//!
//! These helpers are exercised by `listing_tests` and reserved for a future
//! workflow-listing surface; the product path currently formats
//! `registry::WorkflowListing`. Deviation D-11 in
//! `docs/DEVIATIONS.md` tracks the gap.

#![allow(dead_code)]

use std::cmp::Ordering;

use serde::Deserialize;
use serde::Serialize;

/// `description + when_to_use` is truncated to this many UTF-8 bytes.
pub(crate) const MAX_LISTING_SUMMARY_BYTES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowScope {
    /// Highest resolution priority.
    Bundled,
    Builtin,
    Project,
    User,
}

impl WorkflowScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Builtin => "builtin",
            Self::Project => "project",
            Self::User => "user",
        }
    }

    /// Resolution priority; lower wins.
    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::Bundled => 0,
            Self::Builtin => 1,
            Self::Project => 2,
            Self::User => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ListingEntry {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub when_to_use: Option<String>,
    pub scope: WorkflowScope,
    #[serde(default)]
    pub source: Option<String>,
}

impl ListingEntry {
    pub(crate) fn summary(&self) -> String {
        match &self.when_to_use {
            Some(when) if !when.trim().is_empty() => truncate_utf8(
                &format!("{} — {}", self.description, when),
                MAX_LISTING_SUMMARY_BYTES,
            ),
            _ => truncate_utf8(&self.description, MAX_LISTING_SUMMARY_BYTES),
        }
    }
}

/// Order by resolution priority, then name. The first entry for a name is the
/// one that resolves; later entries are shadowed.
pub(crate) fn sort_entries(entries: &mut [ListingEntry]) {
    entries.sort_by(|left, right| {
        left.scope
            .priority()
            .cmp(&right.scope.priority())
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Mark entries whose name was already claimed by a higher-priority scope.
pub(crate) fn shadowed(entries: &[ListingEntry]) -> Vec<bool> {
    let mut seen = std::collections::BTreeSet::new();
    entries
        .iter()
        .map(|entry| !seen.insert(entry.name.clone()))
        .collect()
}

pub(crate) fn render(entries: &mut [ListingEntry]) -> String {
    sort_entries(entries);
    let shadowed = shadowed(entries);
    let mut out = String::new();
    for (entry, is_shadowed) in entries.iter().zip(shadowed) {
        out.push_str(&format!(
            "- {} [{}]{}: {}\n",
            entry.name,
            entry.scope.as_str(),
            if is_shadowed { " (shadowed)" } else { "" },
            entry.summary()
        ));
    }
    out
}

/// Truncate on a UTF-8 character boundary, appending `…` when shortened.
pub(crate) fn truncate_utf8(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let ellipsis = "…";
    let budget = max.saturating_sub(ellipsis.len());
    let mut end = budget;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", value.get(..end).unwrap_or_default(), ellipsis)
}

/// Exact ordering helper for tests and callers that need the resolution winner.
pub(crate) fn compare_for_resolution(left: &ListingEntry, right: &ListingEntry) -> Ordering {
    left.scope
        .priority()
        .cmp(&right.scope.priority())
        .then_with(|| left.name.cmp(&right.name))
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
