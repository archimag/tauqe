//! Stable prompt inputs and the replaceable file view of a bounded turn.

use crate::context::{ContextAccess, ContextFileContent};
use crate::edits::StagedEditsState;
use crate::history::{HistorySnapshot, TurnRoundRecord};
use crate::llm::CapabilityTree;
use crate::providers::ChatMessage;
use crate::workflow::PatchStrategy;
use indexmap::IndexMap;
use std::path::Path;

/// Identifies one accepted record within its turn, including across reseeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedRoundId(u32);

impl AcceptedRoundId {
    pub fn identifies(self, record: &TurnRoundRecord) -> bool {
        self.0 == record.index
    }
}

#[derive(Debug)]
enum TranscriptEntry {
    Discovery(ChatMessage),
    Feedback(ChatMessage),
    Answer {
        round: TurnRoundRecord,
        scratchpad: String,
    },
}

/// Round transcript of one bounded turn.
///
/// The transcript freezes the byte-stable head of the turn and lets every later
/// round append to what the model has already seen instead of rebuilding the
/// whole message sequence. The caller owns its lifetime and any sharing between
/// rounds; this storage mechanism never schedules a request.
///
/// A change of capabilities can reseed the head. File updates travel at the tail.
#[derive(Debug)]
pub struct TurnTranscript {
    /// First text delivered for each path in this turn, in arrival order.
    originals: IndexMap<String, ContextFileContent>,
    /// Staged destinations that had no file when the turn created them.
    created: std::collections::HashSet<String>,
    /// Number of originals included in the prefix or a subsequent discovery block.
    delivered_originals: usize,
    /// System head of the turn, rendered when the transcript is seeded.
    prefix: Vec<ChatMessage>,
    /// Task message of the turn, frozen for the whole turn.
    task: ChatMessage,
    /// Accepted answers, feedback and discovered files in delivery order.
    tail: Vec<TranscriptEntry>,
    scratchpad_chars: usize,
    /// Serialized capability tree the prefix was seeded with.
    seed_signature: Option<String>,
    /// History rendered into the seeded prefix, including explicit absence.
    seed_history: Option<Option<HistorySnapshot>>,
}

impl Default for TurnTranscript {
    fn default() -> Self {
        Self {
            originals: Default::default(),
            created: Default::default(),
            delivered_originals: 0,
            prefix: Vec::new(),
            task: ChatMessage::user(String::new()),
            tail: Vec::new(),
            scratchpad_chars: 0,
            seed_signature: None,
            seed_history: None,
        }
    }
}

impl TurnTranscript {
    /// Read-only recovery handle for accepted rounds, usable after the task exits.
    pub fn history_reader(
        transcript: &std::sync::Arc<std::sync::Mutex<Self>>,
    ) -> impl FnOnce() -> Vec<TurnRoundRecord> + Send + 'static {
        let transcript = std::sync::Arc::clone(transcript);
        move || {
            transcript
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .rounds()
                .cloned()
                .collect()
        }
    }

    pub(crate) fn remember_files(&mut self, files: &[ContextFileContent]) {
        for file in files {
            if !self.created.contains(&file.path) && !self.originals.contains_key(&file.path) {
                self.originals.insert(file.path.clone(), file.clone());
            }
        }
    }

    /// Seeds the transcript with the rendered head of the turn and its task message.
    ///
    /// The first round of a turn seeds a snapshot of the assembly it was built from,
    /// so the prefix and the task message stay frozen for every round that follows.
    pub fn seed(
        &mut self,
        prefix: Vec<ChatMessage>,
        task: ChatMessage,
        capabilities: &CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) {
        self.reseed_prefix(prefix, capabilities, history);
        self.task = task;
    }

    /// Installs a rendered head containing all originals captured so far.
    ///
    /// A phase transition re-renders the capabilities the turn advertises, so the
    /// new prefix already includes the files in earlier discovery blocks.
    pub fn reseed_prefix(
        &mut self,
        prefix: Vec<ChatMessage>,
        capabilities: &CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) {
        self.seed_signature = Some(serde_json::to_string(capabilities).unwrap_or_default());
        self.seed_history = Some(history.cloned());
        self.prefix = prefix;
        self.delivered_originals = self.originals.len();
        self.tail
            .retain(|entry| !matches!(entry, TranscriptEntry::Discovery(_)));
    }

    /// Returns the stable prefix, task, and original files discovered later.
    pub fn messages(&self) -> Vec<ChatMessage> {
        if !self.is_seeded() {
            return Vec::new();
        }
        let mut messages = Vec::with_capacity(self.prefix.len() + 1 + self.tail.len());
        messages.extend(self.prefix.iter().cloned());
        messages.push(self.task.clone());
        messages.extend(self.tail.iter().filter_map(|entry| match entry {
            TranscriptEntry::Discovery(message) | TranscriptEntry::Feedback(message) => {
                Some(message.clone())
            }
            TranscriptEntry::Answer { round, scratchpad } => {
                let mut content = round.message.clone();
                if !scratchpad.is_empty() {
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str("<scratchpad>");
                    content.push_str(&crate::escape::escape_xml_text(scratchpad));
                    content.push_str("</scratchpad>");
                }
                (!content.is_empty()).then(|| ChatMessage::assistant(content))
            }
        }));
        messages
    }

    /// Projection used when a new grant or history replaces the rendered
    /// prefix. All original files will be in that prefix, not repeated here.
    pub fn reseed_continuation(&self) -> Vec<ChatMessage> {
        let messages = self.messages();
        let mut continuation = vec![self.task.clone()];
        let mut rendered = messages.into_iter().skip(self.prefix.len() + 1);
        for entry in &self.tail {
            let has_message = match entry {
                TranscriptEntry::Answer { round, scratchpad } => {
                    !round.message.is_empty() || !scratchpad.is_empty()
                }
                _ => true,
            };
            if has_message {
                if let Some(message) = rendered.next() {
                    if !matches!(entry, TranscriptEntry::Discovery(_)) {
                        continuation.push(message);
                    }
                }
            }
        }
        continuation
    }

    /// Reports whether the transcript has been seeded with a prefix and a task.
    pub fn is_seeded(&self) -> bool {
        self.seed_signature.is_some()
    }

    /// Reports whether the transcript advertises exactly this capability tree.
    ///
    /// A different tree is the phase boundary of the turn: the head the turn renders
    /// is rewritten once, while the task message and the rounds already exchanged keep
    /// their exact bytes.
    pub fn advertises(
        &self,
        capabilities: &CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) -> bool {
        let signature = serde_json::to_string(capabilities).unwrap_or_default();
        self.seed_signature.as_deref() == Some(signature.as_str())
            && self
                .seed_history
                .as_ref()
                .is_some_and(|seeded| seeded.as_ref() == history)
    }

    /// Adds originals discovered since the last render to the stable sequence.
    pub fn append_discoveries(&mut self, strategy: PatchStrategy) {
        if self.delivered_originals == self.originals.len() {
            return;
        }
        let mut block = String::from("<context>\n");
        for file in self.originals.values().skip(self.delivered_originals) {
            block.push_str(&crate::prompt::render_context_element(
                file,
                file.access,
                strategy,
            ));
        }
        block.push_str("</context>");
        self.tail
            .push(TranscriptEntry::Discovery(ChatMessage::user(block)));
        self.delivered_originals = self.originals.len();
    }

    pub fn accept_round(
        &mut self,
        round: usize,
        message: &str,
        notes: &str,
        limit: usize,
    ) -> AcceptedRoundId {
        let scratchpad: String = notes
            .trim()
            .chars()
            .take(limit.saturating_sub(self.scratchpad_chars))
            .collect();
        self.scratchpad_chars += scratchpad.chars().count();
        let record = crate::history::entry::round_record(round as u32, message, "");
        let id = AcceptedRoundId(record.index);
        self.tail.push(TranscriptEntry::Answer {
            round: record,
            scratchpad,
        });
        id
    }

    pub(crate) fn rounds(&self) -> impl DoubleEndedIterator<Item = &TurnRoundRecord> {
        self.tail.iter().filter_map(|entry| match entry {
            TranscriptEntry::Answer { round, .. } => Some(round),
            _ => None,
        })
    }

    pub fn remember_feedback(&mut self, feedback: &str) {
        if feedback.is_empty() {
            return;
        }
        if let Some(round) = self.tail.iter_mut().rev().find_map(|entry| match entry {
            TranscriptEntry::Answer { round, .. } => Some(round),
            _ => None,
        }) {
            // History shows a short excerpt; the next request receives the full diagnostics.
            round.feedback = feedback.trim().chars().take(240).collect();
        }
        self.tail
            .push(TranscriptEntry::Feedback(ChatMessage::user(feedback)));
    }
}

/// File views for one request. Only original file blocks enter the stable prefix.
pub struct RoundFiles {
    pub originals: Vec<ContextFileContent>,
    pub current: Vec<ContextFileContent>,
    pub current_versions: Option<String>,
}

impl TurnTranscript {
    pub fn remember_created_paths(&mut self, paths: &[String]) {
        for path in paths {
            if !self.originals.contains_key(path) {
                self.created.insert(path.clone());
            }
        }
    }

    pub fn project_files(
        &mut self,
        mut context_files: Vec<ContextFileContent>,
        staged_state: Option<&StagedEditsState>,
        root: &Path,
        strategy: PatchStrategy,
    ) -> RoundFiles {
        self.remember_files(&context_files);
        if let Some(staged) = staged_state {
            context_files.retain(|file| {
                staged
                    .staged_files
                    .get(&file.path)
                    .is_none_or(|bytes| std::str::from_utf8(bytes).is_ok())
            });
            let staged_text = staged.staged_text_files();
            let mut appended = Vec::new();
            for (path, content) in staged_text {
                if let Some(file) = context_files.iter_mut().find(|file| file.path == path) {
                    file.content = content;
                } else {
                    appended.push(ContextFileContent {
                        path,
                        access: ContextAccess::Editable,
                        content,
                        insertion_seq: 0,
                    });
                }
            }
            appended.sort_by(|a, b| a.path.cmp(&b.path));
            context_files.extend(appended);
            context_files.retain(|file| !staged.deleted_paths.contains(&file.path));
        }

        let current_versions = render_current_versions(
            &self.originals,
            &context_files,
            strategy,
            root,
            staged_state,
        );
        RoundFiles {
            originals: self.originals.values().cloned().collect(),
            current: context_files,
            current_versions,
        }
    }
}

/// Renders the current differences from the original file snapshots.
fn render_current_versions(
    originals: &IndexMap<String, ContextFileContent>,
    current: &[ContextFileContent],
    patch_strategy: PatchStrategy,
    root: &Path,
    staged: Option<&StagedEditsState>,
) -> Option<String> {
    use std::collections::BTreeMap;
    let mut changes = BTreeMap::new();
    let current_by_path: BTreeMap<_, _> = current.iter().map(|f| (f.path.as_str(), f)).collect();
    for file in current {
        match originals.get(file.path.as_str()) {
            Some(original)
                if original.content == file.content && original.access == file.access =>
            {
                continue
            }
            Some(original) if original.content == file.content => {
                changes.insert(
                    &file.path,
                    format!(
                        "    <access path=\"{}\" value=\"{}\" />\n",
                        file.path,
                        crate::prompt::access_attribute(file.access)
                    ),
                );
            }
            _ => {
                changes.insert(
                    &file.path,
                    crate::prompt::render_context_element(file, file.access, patch_strategy),
                );
            }
        }
    }
    for original in originals.values() {
        if !current_by_path.contains_key(original.path.as_str()) {
            let status = if !staged
                .is_some_and(|state| state.deleted_paths.contains(&original.path))
                && !matches!(root.join(&original.path).try_exists(), Ok(false))
            {
                "unavailable"
            } else {
                "deleted"
            };
            changes.insert(
                &original.path,
                format!("    <{status} path=\"{}\" />\n", original.path),
            );
        }
    }
    if changes.is_empty() {
        return None;
    }
    let mut block = String::from("<current_versions>\n");
    for element in changes.into_values() {
        block.push_str(&element);
    }
    block.push_str("</current_versions>");
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratchpad_only_replies_share_the_turn_limit_across_prefix_reseeds() {
        let mut transcript = TurnTranscript::default();
        let capabilities = CapabilityTree::from_leaves([crate::llm::LlmCapability::Scratchpad]);
        transcript.seed(Vec::new(), ChatMessage::user("task"), &capabilities, None);
        transcript.accept_round(1, "", "界<", 3);
        transcript.reseed_prefix(Vec::new(), &capabilities, None);
        transcript.accept_round(2, "", "ёx", 3);
        transcript.accept_round(3, "", "extra", 3);
        assert_eq!(
            transcript.messages(),
            [
                ChatMessage::user("task"),
                ChatMessage::assistant("<scratchpad>界&lt;</scratchpad>"),
                ChatMessage::assistant("<scratchpad>ё</scratchpad>"),
            ]
        );
        assert!(transcript
            .rounds()
            .all(|round| round.message.is_empty() && round.feedback.is_empty()));
    }
}
