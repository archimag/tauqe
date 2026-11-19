//! Domain history records and compatibility decoding for stored sessions.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    User,
    Assistant,
    System,
}

/// Lifecycle state of a single [`HistoryItem`] record.
///
/// The model's transaction history and the developer's interactive chronicle
/// are deliberately different views of one session. A prompt becomes a record
/// as soon as the developer sends it, so the interface can always show what the
/// harness is running, while the model receives only the records whose turn
/// committed. This enum is the typed contract that keeps the two apart: the
/// state is never inferred from surrounding events or from the text of a
/// message, and it never travels as a free-form string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HistoryEntryStatus {
    /// The turn is still running: the record is visible to the developer but
    /// has not entered the model context yet.
    Active,
    /// The turn committed its result: the record is authoritative and belongs
    /// to the model context.
    #[default]
    Committed,
    /// The turn was interrupted after at least one completed round: the rounds
    /// it finished stay recorded as durable attempts while no final answer
    /// exists.
    Interrupted,
    /// The turn rolled back: the record stays in the chronicle as a failed
    /// attempt but never reached the model context.
    RolledBack,
}

/// One intermediate round of a bounded turn, as the model produced it.
///
/// A turn that discovers material, retries a rejected patch, or heals a
/// compiler error answers several times before its final answer. Only those
/// intermediate rounds travel here: the final answer stays the ordinary
/// assistant record of the same turn, so the chronicle never shows one answer
/// twice. The message is the cleaned assistant text — never the raw protocol
/// payload — and `feedback` is the compact summary of what the harness returned
/// to open the round that follows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRoundRecord {
    /// One-based position of the round inside the turn.
    pub index: u32,
    /// Cleaned assistant message the round produced.
    pub message: String,
    /// Compact summary of the feedback that opened the following round.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub feedback: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    pub id: u64,
    pub kind: HistoryKind,
    pub text: String,
    pub summary: Option<String>,
    pub commit_hash: Option<String>,
    pub files: Vec<HistoryFileChange>,
    /// Intermediate rounds the recorded turn ran before its final answer.
    ///
    /// Only a turn record carries them, and only the rounds that never became
    /// the recorded answer.
    pub rounds: Vec<TurnRoundRecord>,
    /// Lifecycle state of the record, see [`HistoryEntryStatus`].
    pub status: HistoryEntryStatus,
}

/// One file a history entry touched, as a typed record rather than display text.
///
/// The turn knows deterministically which operation it performed on a path, so
/// the record carries the path together with the operation. Flattening the operation into a display prefix would
/// force the presentation layer to re-infer typed knowledge from text, where a
/// wording change or a path that itself begins with a prefix silently
/// misclassifies the entry without any compiler noticing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryFileChange {
    /// Repository-relative path the entry touched; the move source for a move.
    pub path: String,
    /// Operation the turn performed, absent for a plain path listing such as a
    /// context file or a post-hook artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<HistoryFileOperation>,
    /// Move destination, present only when `op` is [`HistoryFileOperation::Move`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// Retired display prefix of a move record, whose remainder is `from -> to`.
const LEGACY_MOVE_PREFIX: &str = "[MOVE] ";

/// Separator between the move source and its destination in the retired format.
const LEGACY_MOVE_ARROW: &str = " -> ";

/// Retired display prefixes and the operations they denoted.
///
/// The prefixes are disjoint, so the table is a direct decoding of the format
/// that existing sessions already contain on disk.
const LEGACY_DISPLAY_PREFIXES: &[(&str, HistoryFileOperation)] = &[
    ("[NEW] ", HistoryFileOperation::Create),
    ("[DEL] ", HistoryFileOperation::Delete),
    ("[OVERWRITE] ", HistoryFileOperation::Overwrite),
    ("[EDIT] ", HistoryFileOperation::Replace),
];

impl HistoryFileChange {
    /// Builds a record that only names a path.
    pub fn plain(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            op: None,
            to: None,
        }
    }

    /// Builds a record for a path the turn modified with `op`.
    pub(crate) fn operated(op: HistoryFileOperation, path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            op: Some(op),
            to: None,
        }
    }

    /// Builds a record for a move from `from` to `to`.
    pub(crate) fn moved(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            path: from.into(),
            op: Some(HistoryFileOperation::Move),
            to: Some(to.into()),
        }
    }

    /// Builds a record for a directory move from `from` to `to`.
    pub fn moved_dir(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            path: from.into(),
            op: Some(HistoryFileOperation::MoveDir),
            to: Some(to.into()),
        }
    }

    /// Decodes one file entry written by the retired display-prefix format.
    ///
    /// Compatibility note (Conventions §1.5): history records persisted before
    /// this type existed flattened the file operation into display prefixes
    /// (`[NEW] `, `[DEL] `, `[OVERWRITE] `, `[MOVE] from -> to`, `[EDIT] `).
    /// This parser exists solely to migrate those already-stored strings into
    /// typed records on read. It never participates in writing or rendering new
    /// data: the operation is authored once by the producer and travels typed.
    ///
    /// A legacy string that genuinely begins with a prefix is indistinguishable
    /// from an annotated one, which is exactly why the format is retired; such a
    /// value decodes into the operation its prefix names. Anything unrecognized
    /// falls back to a plain record so a foreign entry never fails to load.
    pub(crate) fn from_legacy_display(raw: &str) -> Self {
        if let Some(rest) = raw.strip_prefix(LEGACY_MOVE_PREFIX) {
            if let Some((from, to)) = rest.split_once(LEGACY_MOVE_ARROW) {
                return Self::moved(from, to);
            }
        }
        for (prefix, op) in LEGACY_DISPLAY_PREFIXES {
            if let Some(path) = raw.strip_prefix(prefix) {
                return Self::operated(*op, path);
            }
        }
        Self::plain(raw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum HistoryFileOperation {
    Replace,
    Create,
    Delete,
    Move,
    /// Records a directory subtree move.
    MoveDir,
    Overwrite,
}

impl TryFrom<String> for HistoryFileOperation {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "replace" => Ok(Self::Replace),
            "create" => Ok(Self::Create),
            "delete" => Ok(Self::Delete),
            "move" => Ok(Self::Move),
            "move_dir" => Ok(Self::MoveDir),
            "overwrite" => Ok(Self::Overwrite),
            other => Err(format!(
                "Unknown change operation '{}'. Expected replace, create, delete, move, move_dir, or overwrite",
                other
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_history_file_change_from_legacy_display_prefixes() {
        assert_eq!(
            HistoryFileChange::from_legacy_display("[NEW] src/new.rs"),
            HistoryFileChange::operated(HistoryFileOperation::Create, "src/new.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[DEL] src/old.rs"),
            HistoryFileChange::operated(HistoryFileOperation::Delete, "src/old.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[OVERWRITE] src/gen.rs"),
            HistoryFileChange::operated(HistoryFileOperation::Overwrite, "src/gen.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[EDIT] src/lib.rs"),
            HistoryFileChange::operated(HistoryFileOperation::Replace, "src/lib.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] src/a.rs -> src/b.rs"),
            HistoryFileChange::moved("src/a.rs", "src/b.rs")
        );
    }

    #[test]
    fn test_history_file_change_from_legacy_display_move_arrow_split() {
        // The split happens on the first arrow, so an arrow inside the source or
        // destination path does not shift where the destination begins.
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] a -> b -> c"),
            HistoryFileChange::moved("a", "b -> c")
        );

        // A move marker without an arrow is not a move and stays a plain record.
        assert_eq!(
            HistoryFileChange::from_legacy_display("[MOVE] src/a.rs"),
            HistoryFileChange::plain("[MOVE] src/a.rs")
        );
    }

    #[test]
    fn test_history_file_change_from_legacy_display_falls_back_to_plain() {
        assert_eq!(
            HistoryFileChange::from_legacy_display("src/plain.rs"),
            HistoryFileChange::plain("src/plain.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display("[EDIT]src/no-space.rs"),
            HistoryFileChange::plain("[EDIT]src/no-space.rs")
        );
        assert_eq!(
            HistoryFileChange::from_legacy_display(""),
            HistoryFileChange::plain("")
        );
    }
}
