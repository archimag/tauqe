//! Stable prompt inputs and the replaceable file view of a bounded turn.

use crate::context::{ContextAccess, ContextFileContent};
use crate::edits::StagedEditsState;
use crate::history::HistorySnapshot;
use crate::llm::CapabilityTree;
use crate::providers::ChatMessage;
use crate::workflow::PatchStrategy;
use indexmap::IndexMap;
use std::path::Path;

/// Round transcript of one bounded turn.
///
/// The transcript freezes the byte-stable head of the turn and lets every later
/// round append to what the model has already seen instead of rebuilding the
/// whole message sequence. It is shared through [`super::context::LoopContext`], so a round a
/// reborrowed child context appends stays visible to the context it came from.
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
    /// Original file blocks discovered after the task was sent.
    added_context: Vec<ChatMessage>,
    /// Feedback the next round appends as its user message.
    pending_feedback: Option<String>,
    /// Serialized capability tree the prefix was seeded with.
    seed_signature: Option<String>,
    /// History rendered into the seeded prefix, including explicit absence.
    seed_history: Option<Option<HistorySnapshot>>,
    /// The latest answer for the round-budget outcome.
    pub(super) last_answer: Option<String>,
}

impl Default for TurnTranscript {
    fn default() -> Self {
        Self {
            originals: Default::default(),
            created: Default::default(),
            delivered_originals: 0,
            prefix: Vec::new(),
            task: ChatMessage::user(String::new()),
            added_context: Vec::new(),
            pending_feedback: None,
            seed_signature: None,
            seed_history: None,
            last_answer: None,
        }
    }
}

impl TurnTranscript {
    pub(super) fn remember_files(&mut self, files: &[ContextFileContent]) {
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
    pub(crate) fn seed(
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
    pub(crate) fn reseed_prefix(
        &mut self,
        prefix: Vec<ChatMessage>,
        capabilities: &CapabilityTree,
        history: Option<&HistorySnapshot>,
    ) {
        self.seed_signature = Some(serde_json::to_string(capabilities).unwrap_or_default());
        self.seed_history = Some(history.cloned());
        self.prefix = prefix;
        self.delivered_originals = self.originals.len();
        self.added_context.clear();
    }

    /// Returns the stable prefix, task, and original files discovered later.
    pub(crate) fn messages(&self) -> Vec<ChatMessage> {
        if !self.is_seeded() {
            return Vec::new();
        }
        let mut messages = Vec::with_capacity(self.prefix.len() + 1 + self.added_context.len());
        messages.extend(self.prefix.iter().cloned());
        messages.push(self.task.clone());
        messages.extend(self.added_context.iter().cloned());
        messages
    }

    /// Reports whether the transcript has been seeded with a prefix and a task.
    pub(crate) fn is_seeded(&self) -> bool {
        self.seed_signature.is_some()
    }

    /// Reports whether the transcript advertises exactly this capability tree.
    ///
    /// A different tree is the phase boundary of the turn: the head the turn renders
    /// is rewritten once, while the task message and the rounds already exchanged keep
    /// their exact bytes.
    pub(crate) fn advertises(
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
    pub(super) fn append_discoveries(&mut self, strategy: PatchStrategy) {
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
        self.added_context.push(ChatMessage::user(block));
        self.delivered_originals = self.originals.len();
    }

    /// Takes the feedback the next round appends as its user message.
    pub(crate) fn take_pending_feedback(&mut self) -> Option<String> {
        self.pending_feedback.take()
    }

    /// Records the feedback the next round appends as its user message.
    ///
    /// The slot is written by every producer of an additional round — the
    /// research journal, verification healing, patch retry, and the empty-round
    /// reminder — and taken exactly once by the round that appends the pair, so
    /// feedback never reaches the provider twice. Blank feedback carries no
    /// knowledge and leaves the slot untouched.
    pub(crate) fn set_pending_feedback(&mut self, feedback: impl Into<String>) {
        let feedback = feedback.into();
        if feedback.trim().is_empty() {
            return;
        }
        self.pending_feedback = Some(feedback);
    }
}

/// File views for one request. Only original file blocks enter the stable prefix.
pub(super) struct RoundFiles {
    pub originals: Vec<ContextFileContent>,
    pub current: Vec<ContextFileContent>,
    pub current_versions: Option<String>,
}

impl TurnTranscript {
    pub(super) fn remember_staged_paths(&mut self, root: &Path, staged: &StagedEditsState) {
        for path in staged.staged_files.keys() {
            if !self.originals.contains_key(path) && !root.join(path).exists() {
                self.created.insert(path.clone());
            }
        }
    }

    pub(super) fn project_files(
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
