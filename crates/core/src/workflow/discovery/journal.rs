//! Cumulative epistemic journal of a bounded turn.
//!
//! Discovery rounds widen what the model knows: they return symbol outlines,
//! search matches, directory listings, and documentation. Those findings are
//! not repository files, so they must never be mixed into the authoritative
//! `<context>` section, where they would pollute search/replace patching and
//! break the byte-stable prompt prefix.
//!
//! This journal keeps them as an ordered, append-only tail of the user message.
//! It survives the whole turn, so the outlines requested during Discovery stay
//! available while the model repairs patches in Staging and compiler errors in
//! Verification Healing. New knowledge is only ever appended, and an identical
//! repeated block is not appended twice, so re-requesting the same outline never
//! inflates the tail.

use super::DiscoveryRoundOutcome;

/// Opening marker of the epistemic tail.
const JOURNAL_OPEN: &str = "<discovery_journal>";
/// Closing marker of the epistemic tail.
const JOURNAL_CLOSE: &str = "</discovery_journal>";

/// Ordered, append-only collection of epistemic feedback blocks of one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnDiscoveryJournal {
    entries: Vec<String>,
}

impl TurnDiscoveryJournal {
    /// Appends one feedback block.
    ///
    /// Blank blocks carry no knowledge and are dropped. A block identical to one
    /// already recorded is dropped too: the turn tail only ever grows with
    /// genuinely new findings.
    pub fn push_block(&mut self, block: impl Into<String>) {
        let block = block.into();
        let block = block.trim();
        if block.is_empty() {
            return;
        }
        if self.entries.iter().any(|existing| existing == block) {
            return;
        }
        self.entries.push(block.to_string());
    }

    /// Appends every feedback block produced by one discovery round.
    ///
    /// The round outcome is read-only here: the journal observes what the round
    /// learned and never rewrites the context the round already applied.
    pub fn record_outcome(&mut self, outcome: &DiscoveryRoundOutcome) {
        for block in &outcome.feedback_blocks {
            self.push_block(block.as_str());
        }
    }

    /// Reports whether the journal holds no findings.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the recorded blocks in the order they were learned.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Renders the whole journal as one delimited tail, or `None` when empty.
    pub fn rendered(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str(JOURNAL_OPEN);
        for entry in &self.entries {
            out.push('\n');
            out.push_str(entry);
        }
        out.push('\n');
        out.push_str(JOURNAL_CLOSE);
        Some(out)
    }

    /// Composes the user message of a turn round.
    ///
    /// The order is fixed: the original task first, the accumulated epistemic
    /// tail next, the diagnostics of the current phase last. Nothing is inserted
    /// before the task, so the stable part of the message keeps its bytes and the
    /// provider prefix cache stays valid.
    pub fn compose_user_prompt(
        &self,
        base_prompt: &str,
        phase_diagnostics: Option<&str>,
    ) -> String {
        let mut out = String::new();
        out.push_str(base_prompt.trim_end());

        if let Some(tail) = self.rendered() {
            out.push_str("\n\n");
            out.push_str(&tail);
        }

        if let Some(diagnostics) = phase_diagnostics {
            let diagnostics = diagnostics.trim();
            if !diagnostics.is_empty() {
                out.push_str("\n\n");
                out.push_str(diagnostics);
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome_with(blocks: &[&str]) -> DiscoveryRoundOutcome {
        let mut outcome = DiscoveryRoundOutcome::default();
        for block in blocks {
            outcome.feedback_blocks.push((*block).to_string());
        }
        outcome
    }

    #[test]
    fn test_journal_appends_blocks_in_order() {
        let mut journal = TurnDiscoveryJournal::default();
        assert!(journal.is_empty());

        journal.push_block("<repomap_feedback path=\"src\">\npub fn alpha\n</repomap_feedback>");
        journal.push_block(
            "<search_feedback path=\".\" regex=\"beta\">\n2 matches\n</search_feedback>",
        );

        assert!(!journal.is_empty());
        assert_eq!(journal.entries().len(), 2);
        assert!(journal.entries()[0].starts_with("<repomap_feedback"));
        assert!(journal.entries()[1].starts_with("<search_feedback"));
    }

    #[test]
    fn test_journal_ignores_blank_blocks() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.push_block("   \n\t ");
        journal.push_block("");
        assert!(journal.is_empty());
        assert_eq!(journal.rendered(), None);
    }

    #[test]
    fn test_journal_deduplicates_repeated_blocks() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.push_block("<repo_dirs_feedback>\nsrc\n</repo_dirs_feedback>");
        journal.push_block("  <repo_dirs_feedback>\nsrc\n</repo_dirs_feedback>  ");
        journal.push_block("<repo_dirs_feedback>\nsrc\n  nested\n</repo_dirs_feedback>");

        assert_eq!(journal.entries().len(), 2);
    }

    #[test]
    fn test_journal_records_round_outcome_without_duplicates() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.record_outcome(&outcome_with(&[
            "<system_documentation>\nA\n</system_documentation>",
        ]));
        journal.record_outcome(&outcome_with(&[
            "<system_documentation>\nA\n</system_documentation>",
            "<dir_list_feedback path=\"src\" recursive=\"false\">\nmain.rs\n</dir_list_feedback>",
        ]));

        assert_eq!(journal.entries().len(), 2);
        assert!(journal.entries()[0].contains("<system_documentation>"));
        assert!(journal.entries()[1].contains("<dir_list_feedback"));
    }

    #[test]
    fn test_journal_renders_a_delimited_tail() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.push_block("<context_feedback>\nalready loaded\n</context_feedback>");

        let rendered = journal.rendered().expect("journal must render");
        assert!(rendered.starts_with(JOURNAL_OPEN));
        assert!(rendered.ends_with(JOURNAL_CLOSE));
        assert!(rendered.contains("<context_feedback>"));
    }

    #[test]
    fn test_compose_user_prompt_orders_task_tail_and_diagnostics() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.push_block("<repomap_feedback path=\"src\">\npub fn alpha\n</repomap_feedback>");

        let composed = journal.compose_user_prompt(
            "Implement the journal.\n",
            Some("  compiler error: unresolved import  "),
        );

        let task_at = composed.find("Implement the journal.").expect("task");
        let tail_at = composed.find(JOURNAL_OPEN).expect("tail");
        let diagnostics_at = composed.find("compiler error").expect("diagnostics");

        assert!(task_at < tail_at);
        assert!(tail_at < diagnostics_at);
        // Trailing whitespace of the task is trimmed, the task keeps its text.
        assert!(!composed.contains("journal.\n\n\n"));
    }

    #[test]
    fn test_compose_user_prompt_without_journal_or_diagnostics() {
        let journal = TurnDiscoveryJournal::default();
        assert_eq!(
            journal.compose_user_prompt("Base task.", None),
            "Base task."
        );
        assert_eq!(
            journal.compose_user_prompt("Base task.", Some("   ")),
            "Base task."
        );
    }

    #[test]
    fn test_compose_user_prompt_with_journal_only() {
        let mut journal = TurnDiscoveryJournal::default();
        journal.push_block("<system_documentation>\ncontext\n</system_documentation>");

        let composed = journal.compose_user_prompt("Base task.", None);
        assert!(composed.starts_with("Base task.\n\n"));
        assert!(composed.ends_with(JOURNAL_CLOSE));
    }
}
