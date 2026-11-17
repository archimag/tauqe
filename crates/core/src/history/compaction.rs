use super::entry::HistoryEntry;
use tauqe_protocol::TurnRoundRecord;

/// Default budget of tokens for interaction history before compaction is triggered.
pub(crate) const DEFAULT_HISTORY_BUDGET_TOKENS: u64 = 20_000;

/// Default count of user turns to preserve in detailed form in the tail.
pub(crate) const DEFAULT_TAIL_TURNS_COUNT: usize = 10;

/// Partitions history entries into `(head, tail)` where `tail` contains the last `keep_tail_turns` user turns.
pub(crate) fn partition_head_tail(
    entries: &[HistoryEntry],
    keep_tail_turns: usize,
) -> Option<(Vec<HistoryEntry>, Vec<HistoryEntry>)> {
    if entries.is_empty() {
        return None;
    }

    // Find indices of all UserTurn entries
    let turn_indices: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| match entry {
            HistoryEntry::Turn { .. } => Some(idx),
            _ => None,
        })
        .collect();

    if turn_indices.len() <= keep_tail_turns {
        return None;
    }

    let cutoff_turn_idx = turn_indices.len() - keep_tail_turns;
    let split_pos = turn_indices[cutoff_turn_idx];

    if split_pos == 0 {
        return None;
    }

    let head = entries[..split_pos].to_vec();
    let tail = entries[split_pos..].to_vec();

    Some((head, tail))
}

/// Constructs compacted entries combining a single summary header with preserved tail entries.
///
/// The retained tail is never split: the partition breaks the session at a turn
/// record, so a retained turn keeps the rounds it nested and the records stay in
/// the order they were recorded.
pub(crate) fn build_compacted_history(
    summary_id: Option<u64>,
    summary_text: String,
    tail: Vec<HistoryEntry>,
) -> Vec<HistoryEntry> {
    let mut compacted = Vec::with_capacity(tail.len() + 1);
    compacted.push(HistoryEntry::Summary {
        id: summary_id,
        text: summary_text,
    });
    compacted.extend(tail);
    compacted
}

/// Compact `[Round N]` projection of one recorded round.
///
/// A turn nests the rounds it ran before its final answer, and both the
/// transcript of a later turn and the source of a compaction replay them. The
/// projection states the round position and the cleaned message alone: the
/// recorded payload carries no raw protocol JSON, and the feedback the harness
/// reported back to the round stays out of the replay.
pub(crate) fn round_replay(round: &TurnRoundRecord) -> String {
    format!("[Round {}] {}", round.index, round.message.trim())
}
///
/// A record travels as its journal line, so the Scribe reads what the session
/// persisted. A turn that nested intermediate rounds contributes its own line
/// without the array, followed by one replay line per round and the feedback the
/// round received: those three fields are exactly what a round recorded, so the
/// narrative loses nothing while it states the attempt in the words a later turn
/// reads — what the round did, and what the harness answered it.
fn render_compaction_source(entries: &[HistoryEntry]) -> String {
    let mut source = String::new();
    for entry in entries {
        let rounds: &[TurnRoundRecord] = match entry {
            HistoryEntry::Turn { rounds, .. } => rounds.as_slice(),
            _ => &[],
        };
        if rounds.is_empty() {
            if let Ok(line) = entry.to_jsonl_line() {
                source.push_str(&line);
                source.push('\n');
            }
            continue;
        }

        // The turn states its attempts as a replay instead of a nested array, so
        // the Scribe reads the attempt rather than a payload.
        let mut bare = entry.clone();
        if let HistoryEntry::Turn { rounds, .. } = &mut bare {
            rounds.clear();
        }
        if let Ok(line) = bare.to_jsonl_line() {
            source.push_str(&line);
            source.push('\n');
        }
        for round in rounds {
            source.push_str(&round_replay(round));
            source.push('\n');
            let feedback = round.feedback.trim();
            if !feedback.is_empty() {
                source.push_str("  Feedback: ");
                source.push_str(feedback);
                source.push('\n');
            }
        }
    }
    source
}

/// Builds the request a compaction sends to the Scribe.
///
/// The task names the attempts explicitly, because they are the part of a long
/// turn a summary must not flatten: a later turn needs to know what was already
/// verified and what was already corrected before it continues.
pub(crate) fn build_compaction_request(entries: &[HistoryEntry]) -> String {
    format!(
        "Summarize the following interaction history into a concise, structured summary capturing the completed tasks, key code changes, and context:\n\n```text\n{}\n```\nA turn record may carry the intermediate rounds it ran before its final answer as `[Round N]` lines together with the feedback each round received. State per turn what the rounds verified and what they corrected, so the attempts survive the summary. Respond with a single JSON object matching the enforced schema, placing the summary text in the `summary` field. Do not wrap the JSON in markdown code fences.",
        render_compaction_source(entries),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_head_tail() {
        let entries = vec![
            HistoryEntry::Summary {
                id: None,
                text: "Previous work".to_string(),
            },
            HistoryEntry::Turn {
                id: None,
                prompt: "turn 1".to_string(),
                context: vec![],
                context_watermark: None,
                rounds: Vec::new(),
            },
            HistoryEntry::Response {
                id: None,
                message: "resp 1".to_string(),
                commit: None,
                summary: None,
                files: vec![],
            },
            HistoryEntry::Turn {
                id: None,
                prompt: "turn 2".to_string(),
                context: vec![],
                context_watermark: None,
                rounds: Vec::new(),
            },
            HistoryEntry::Response {
                id: None,
                message: "resp 2".to_string(),
                commit: None,
                summary: None,
                files: vec![],
            },
            HistoryEntry::Turn {
                id: None,
                prompt: "turn 3".to_string(),
                context: vec![],
                context_watermark: None,
                rounds: Vec::new(),
            },
            HistoryEntry::Response {
                id: None,
                message: "resp 3".to_string(),
                commit: None,
                summary: None,
                files: vec![],
            },
            HistoryEntry::Turn {
                id: None,
                prompt: "turn 4".to_string(),
                context: vec![],
                context_watermark: None,
                rounds: Vec::new(),
            },
            HistoryEntry::Response {
                id: None,
                message: "resp 4".to_string(),
                commit: None,
                summary: None,
                files: vec![],
            },
        ];

        // Keep last 2 turns: turns 3 and 4 should be in tail
        let (head, tail) = partition_head_tail(&entries, 2).unwrap();

        assert_eq!(tail.len(), 4); // turn 3, resp 3, turn 4, resp 4
        assert_eq!(tail[0], entries[5]); // turn 3
        assert_eq!(head.len(), 5); // summary, turn 1, resp 1, turn 2, resp 2

        // When turns <= keep_tail_turns, nothing to partition
        assert!(partition_head_tail(&entries, 4).is_none());
        assert!(partition_head_tail(&entries, 5).is_none());
    }

    #[test]
    fn test_build_compacted_history() {
        let tail = vec![HistoryEntry::Turn {
            id: Some(5),
            prompt: "latest".to_string(),
            context: vec![],
            context_watermark: None,
            rounds: Vec::new(),
        }];
        let compacted = build_compacted_history(Some(1), "Summary text".to_string(), tail.clone());
        assert_eq!(compacted.len(), 2);
        assert_eq!(
            compacted[0],
            HistoryEntry::Summary {
                id: Some(1),
                text: "Summary text".to_string()
            }
        );
        assert_eq!(compacted[1], tail[0]);
    }

    /// Builds a turn record that nested the rounds it ran before its answer.
    fn turn_with_rounds(id: u64, prompt: &str, rounds: Vec<(u32, &str, &str)>) -> HistoryEntry {
        HistoryEntry::Turn {
            id: Some(id),
            prompt: prompt.to_string(),
            context: Vec::new(),
            context_watermark: None,
            rounds: rounds
                .into_iter()
                .map(|(index, message, feedback)| TurnRoundRecord {
                    index,
                    message: message.to_string(),
                    feedback: feedback.to_string(),
                })
                .collect(),
        }
    }

    /// Builds the final answer of a turn.
    fn response(id: u64, message: &str) -> HistoryEntry {
        HistoryEntry::Response {
            id: Some(id),
            message: message.to_string(),
            commit: None,
            summary: None,
            files: Vec::new(),
        }
    }

    #[test]
    fn compaction_source_replays_the_recorded_rounds() {
        let entries = vec![
            turn_with_rounds(
                1,
                "Add the parser",
                vec![
                    (1, "Staged the parser change.", "verification failed: E0308"),
                    (2, "Corrected the signature.", "verification passed"),
                ],
            ),
            response(2, "Parser added."),
        ];

        let request = build_compaction_request(&entries);

        // The attempt reads round by round: what the round did, and what the
        // harness reported back to it.
        assert!(request.contains("[Round 1] Staged the parser change."));
        assert!(request.contains("Feedback: verification failed: E0308"));
        assert!(request.contains("[Round 2] Corrected the signature."));
        assert!(request.contains("Feedback: verification passed"));

        // The nested array is replaced by the replay, so the rounds are stated
        // once and the turn keeps its recorded form.
        assert!(!request.contains("\"rounds\""));
        assert!(request.contains(r#""prompt":"Add the parser""#));
        assert!(request.contains(r#""message":"Parser added.""#));
    }

    #[test]
    fn compaction_source_keeps_a_turn_without_rounds_unchanged() {
        let entry = turn_with_rounds(1, "Short task", Vec::new());
        let request = build_compaction_request(std::slice::from_ref(&entry));

        // A turn that recorded no attempt travels as its journal line and gets
        // no invented narrative.
        assert!(request.contains(entry.to_jsonl_line().unwrap().as_str()));
        // The request states the replay format itself, so the check reads the
        // rendered source: a turn that recorded no attempt contributes no
        // `[Round N]` line.
        assert!(!request.contains("[Round 1]"));
        assert!(!request.contains("\"rounds\""));
    }

    #[test]
    fn compaction_folds_the_attempts_and_keeps_the_retained_turn_whole() {
        let entries = vec![
            turn_with_rounds(1, "First task", vec![(1, "Checked the parser.", "E0308")]),
            response(2, "First done."),
            turn_with_rounds(3, "Second task", vec![(1, "Checked the lexer.", "E0425")]),
            response(4, "Second done."),
            turn_with_rounds(5, "Third task", vec![(1, "Checked the codec.", "")]),
        ];

        // The split lands on a turn record, so a turn is never separated from
        // the rounds it nested.
        let (head, tail) = partition_head_tail(&entries, 1).expect("the session folds");
        assert!(matches!(head.last(), Some(HistoryEntry::Response { .. })));
        assert!(matches!(tail.first(), Some(HistoryEntry::Turn { .. })));

        // The summarized prefix states both folded attempts.
        let request = build_compaction_request(&head);
        assert!(request.contains("[Round 1] Checked the parser."));
        assert!(request.contains("[Round 1] Checked the lexer."));
        assert!(!request.contains("Third task"));

        // The retained turn keeps the rounds it recorded while the summarized
        // prefix collapses into one summary record.
        let compacted =
            build_compacted_history(Some(4), "Earlier tasks summarized.".to_string(), tail);
        assert_eq!(compacted.len(), 2);
        match &compacted[1] {
            HistoryEntry::Turn { prompt, rounds, .. } => {
                assert_eq!(prompt, "Third task");
                assert_eq!(rounds.len(), 1);
            }
            other => panic!("expected the retained turn, got {:?}", other),
        }
    }
}
