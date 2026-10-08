use super::entry::HistoryEntry;

/// Default estimated tokens budget before compaction is triggered (raised for modern LLMs).
pub const DEFAULT_HISTORY_BUDGET_TOKENS: u64 = 20_000;

/// Default count of user turns to preserve in detailed form in the tail.
pub const DEFAULT_TAIL_TURNS_COUNT: usize = 10;

/// Estimates tokens used by history entries using the project standard (bytes + 3) / 4.
pub fn estimate_entries_tokens(entries: &[HistoryEntry]) -> u64 {
    let mut total_bytes = 0u64;
    for entry in entries {
        if let Ok(line) = entry.to_jsonl_line() {
            total_bytes += (line.len() + 1) as u64; // + 1 for newline
        }
    }
    total_bytes.div_ceil(4)
}

/// Partitions entries into `(head, tail)` based on preserving the last `keep_tail_turns` user turns.
///
/// A turn boundary starts with `HistoryEntry::Turn`. Everything from the target turn to
/// the end belongs to the tail. All prior entries form the head to be summarized.
/// Returns `None` if there is no head to summarize (e.g. fewer turns than `keep_tail_turns`).
pub fn partition_head_tail(
    entries: &[HistoryEntry],
    keep_tail_turns: usize,
) -> Option<(Vec<HistoryEntry>, Vec<HistoryEntry>)> {
    if keep_tail_turns == 0 || entries.is_empty() {
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
pub fn build_compacted_history(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_head_tail() {
        let entries = vec![
            HistoryEntry::Summary { id: None, text: "Previous work".to_string() },
            HistoryEntry::Turn { id: None, prompt: "turn 1".to_string(), context: vec![] },
            HistoryEntry::Response { id: None, message: "resp 1".to_string(), commit: None, summary: None, files: vec![] },
            HistoryEntry::Turn { id: None, prompt: "turn 2".to_string(), context: vec![] },
            HistoryEntry::Response { id: None, message: "resp 2".to_string(), commit: None, summary: None, files: vec![] },
            HistoryEntry::Turn { id: None, prompt: "turn 3".to_string(), context: vec![] },
            HistoryEntry::Response { id: None, message: "resp 3".to_string(), commit: None, summary: None, files: vec![] },
            HistoryEntry::Turn { id: None, prompt: "turn 4".to_string(), context: vec![] },
            HistoryEntry::Response { id: None, message: "resp 4".to_string(), commit: None, summary: None, files: vec![] },
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
        let tail = vec![
            HistoryEntry::Turn { id: Some(5), prompt: "latest".to_string(), context: vec![] },
        ];
        let compacted = build_compacted_history(Some(1), "Summary text".to_string(), tail.clone());
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0], HistoryEntry::Summary { id: Some(1), text: "Summary text".to_string() });
        assert_eq!(compacted[1], tail[0]);
    }
}
