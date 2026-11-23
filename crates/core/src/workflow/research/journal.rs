//! Cumulative epistemic journal of a bounded turn.
//!
//! Research rounds widen what the model knows: they return symbol outlines,
//! search matches, directory listings, and documentation. Those findings are
//! not repository files, so they must never be mixed into the authoritative
//! `<context>` section, where they would pollute search/replace patching and
//! break the byte-stable prompt prefix.
//!
//! This journal keeps them as an ordered, append-only tail of the user message.
//! It survives the whole turn, so the outlines requested during Research stay
//! available while the model repairs patches in Staging and compiler errors in
//! Verification Healing. New knowledge is only ever appended, and an identical
//! repeated block is not appended twice, so re-requesting the same outline never
//! inflates the tail.
//!
//! A round is also the unit of feedback. [`TurnResearchJournal::begin_round`]
//! marks its boundary and [`TurnResearchJournal::render_round_feedback`]
//! renders only the records the open round produced, so the model is told what
//! its own round delivered instead of receiving the whole turn's investigation
//! again. The prompt builder delivers effective file versions alongside these findings.

use super::ResearchRoundOutcome;

/// Opening marker of the epistemic tail.
const JOURNAL_OPEN: &str = "<research_journal>";
/// Closing marker of the epistemic tail.
const JOURNAL_CLOSE: &str = "</research_journal>";

/// Ordered, append-only collection of epistemic feedback blocks of one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnResearchJournal {
    entries: Vec<String>,
    round: RoundSection,
}

/// Records produced by the research round currently open.
///
/// A round is the unit of feedback: the results, listings, and diagnostics one
/// investigation round reported, together with the files it delivered to the
/// context. The section is replaced at every round boundary, so rendering it
/// never repeats what an earlier round already told the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RoundSection {
    /// Ordinal the loop gave the round, absent before the first boundary.
    index: Option<usize>,
    /// Feedback blocks recorded since the boundary.
    blocks: Vec<String>,
}

impl TurnResearchJournal {
    /// Marks the boundary of the research round the loop just opened.
    ///
    /// Everything recorded after this call belongs to the new round, and
    /// [`Self::render_round_feedback`] renders that round alone. The cumulative
    /// tail keeps the whole turn's investigation, so the boundary narrows what a
    /// round reports without losing what an earlier round learned.
    pub fn begin_round(&mut self, index: usize) {
        self.round = RoundSection {
            index: Some(index),
            ..RoundSection::default()
        };
    }

    pub(crate) fn round_index(&self) -> usize {
        self.round.index.unwrap_or(0)
    }

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
        self.round.blocks.push(block.to_string());
    }

    /// Records the findings of one round.
    pub fn record_outcome(&mut self, outcome: &ResearchRoundOutcome) {
        for block in &outcome.feedback_blocks {
            self.push_block(block.as_str());
        }
    }

    /// Renders findings from the current round. File versions are delivered by the transcript.
    pub fn render_round_feedback(&self) -> String {
        self.round.blocks.join("\n\n")
    }

    /// Reports whether the journal holds no findings.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the recorded blocks in the order they were learned.
    #[cfg(test)]
    fn entries(&self) -> &[String] {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome_with(blocks: &[&str]) -> ResearchRoundOutcome {
        let mut outcome = ResearchRoundOutcome::default();
        for block in blocks {
            outcome.feedback_blocks.push((*block).to_string());
        }
        outcome
    }

    #[test]
    fn test_journal_appends_blocks_in_order() {
        let mut journal = TurnResearchJournal::default();
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
        let mut journal = TurnResearchJournal::default();
        journal.push_block("   \n\t ");
        journal.push_block("");
        assert!(journal.is_empty());
        assert_eq!(journal.rendered(), None);
    }

    #[test]
    fn test_journal_deduplicates_repeated_blocks() {
        let mut journal = TurnResearchJournal::default();
        journal.push_block("<repo_dirs_feedback>\nsrc\n</repo_dirs_feedback>");
        journal.push_block("  <repo_dirs_feedback>\nsrc\n</repo_dirs_feedback>  ");
        journal.push_block("<repo_dirs_feedback>\nsrc\n  nested\n</repo_dirs_feedback>");

        assert_eq!(journal.entries().len(), 2);
    }

    #[test]
    fn test_journal_records_round_outcome_without_duplicates() {
        let mut journal = TurnResearchJournal::default();
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
        let mut journal = TurnResearchJournal::default();
        journal.push_block("<context_feedback>\nalready loaded\n</context_feedback>");

        let rendered = journal.rendered().expect("journal must render");
        assert!(rendered.starts_with(JOURNAL_OPEN));
        assert!(rendered.ends_with(JOURNAL_CLOSE));
        assert!(rendered.contains("<context_feedback>"));
    }

    /// A round renders only the records it produced, so the feedback of round
    /// N never repeats the results of round N-1.
    #[test]
    fn test_round_feedback_covers_only_the_open_round() {
        let mut journal = TurnResearchJournal::default();
        journal.begin_round(1);
        journal.record_outcome(&outcome_with(&[
            "<search_feedback path=\".\" regex=\"alpha\">\n1 match\n</search_feedback>",
        ]));
        let first = journal.render_round_feedback();
        assert!(first.contains("regex=\"alpha\""), "{first}");

        journal.begin_round(2);
        journal.record_outcome(&outcome_with(&[
            "<dir_list_feedback path=\"src\" recursive=\"false\">\nmain.rs\n</dir_list_feedback>",
        ]));
        let second = journal.render_round_feedback();
        assert!(second.contains("<dir_list_feedback"), "{second}");
        assert!(
            !second.contains("regex=\"alpha\""),
            "the feedback of round 2 must not repeat round 1: {second}"
        );

        // The cumulative tail keeps the whole turn's investigation, so a
        // boundary narrows the round without losing an earlier finding.
        let tail = journal.rendered().expect("the tail renders");
        assert!(tail.contains("regex=\"alpha\""), "{tail}");
        assert!(tail.contains("<dir_list_feedback"), "{tail}");
    }
}
