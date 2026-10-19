use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::theme::engine::current_engine;

use super::render::indicator_style;

/// Number of trailing reasoning lines kept visible while a reasoning stream is
/// active. The live stream acts as a progress indicator; the full transcript is
/// rendered once via [`ReasoningState::finalize`].
const REASONING_TAIL_LINES: usize = 10;

/// Hard cap on the raw reasoning transcript retained for the current turn.
///
/// A reasoning model streams for minutes; without a bound the raw text grows for
/// the whole turn and the finalized Markdown render has to parse all of it at
/// once. The transcript is therefore a rolling window: the oldest complete lines
/// are dropped once the cap is crossed, and the dropped prefix is accounted for
/// so the block can state how much of the reasoning was discarded (c-1).
const REASONING_MAX_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct ReasoningState {
    pub text: String,
    pub lines: Vec<Line<'static>>,
    pub show: bool,
    pub theme_mode: crate::config::ThemeMode,
    pub max_width: Option<usize>,
    streaming: bool,
    tail: Vec<Line<'static>>,
    pending: String,
    newline_count: usize,
    tail_consumed: usize,
    truncated_lines: usize,
}

impl Default for ReasoningState {
    fn default() -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            show: true,
            theme_mode: crate::config::ThemeMode::default(),
            max_width: None,
            streaming: false,
            tail: Vec::new(),
            pending: String::new(),
            newline_count: 0,
            tail_consumed: 0,
            truncated_lines: 0,
        }
    }
}

impl ReasoningState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.lines.clear();
        self.show = true;
        self.streaming = false;
        self.tail.clear();
        self.pending.clear();
        self.newline_count = 0;
        self.tail_consumed = 0;
        self.truncated_lines = 0;
    }

    /// Appends a streamed reasoning delta.
    ///
    /// While the stream is active only a lightweight tail window is maintained;
    /// the heavy Markdown render is deferred to [`Self::finalize`].
    pub fn append_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.streaming = true;
        self.enforce_text_budget();
        self.fold_tail();
    }

    /// Trims the oldest lines once the raw transcript crosses the cap.
    ///
    /// The trim is amortized: it triggers only above the cap and drops down to
    /// half of it, so one bounded scan runs per several kilobytes of streamed
    /// reasoning instead of on every delta. Every counter derived from the
    /// dropped prefix is corrected in the same place, so the tail window, the
    /// line counter, and the truncation marker can never disagree.
    fn enforce_text_budget(&mut self) {
        if self.text.len() <= REASONING_MAX_BYTES {
            return;
        }
        let overflow = self.text.len().saturating_sub(REASONING_MAX_BYTES / 2);
        let cut = Self::truncation_offset(&self.text, overflow);
        if cut == 0 {
            return;
        }
        let dropped_lines = self
            .text
            .get(..cut)
            .map_or(0, |head| head.matches('\n').count());
        self.text.drain(..cut);
        self.truncated_lines = self.truncated_lines.saturating_add(dropped_lines);
        self.newline_count = self.newline_count.saturating_sub(dropped_lines);
        self.tail_consumed = self.tail_consumed.saturating_sub(cut);
    }

    /// Byte offset of the first line boundary at or after `overflow`.
    ///
    /// `overflow` is a raw byte count and therefore may fall inside a multi-byte
    /// character; the offset is first advanced to the nearest character
    /// boundary, because slicing anywhere else would panic on Cyrillic, CJK, or
    /// emoji text. A single newline-free line has no boundary to cut on, so the
    /// character boundary itself is the cut point.
    fn truncation_offset(text: &str, overflow: usize) -> usize {
        let start = text
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|idx| *idx >= overflow)
            .unwrap_or(text.len());
        match text.get(start..).and_then(|rest| rest.find('\n')) {
            Some(offset) => start.saturating_add(offset).saturating_add(1),
            None => start,
        }
    }

    /// Finalizes an active reasoning stream by folding its transcript.
    ///
    /// No Markdown is parsed here. A settled stream is folded, and the rendered
    /// lines are built lazily on the next expansion, so a collapsed reasoning
    /// block never pays for thousands of `Span` allocations it cannot show
    /// (c-2). No-op when the stream has already settled.
    pub fn finalize(&mut self) {
        if !self.streaming {
            return;
        }
        self.streaming = false;
        self.show = false;
        self.lines.clear();
    }

    /// Re-renders the reasoning Markdown on demand.
    ///
    /// The AST is built strictly while the block is expanded and settled: a
    /// folded transcript, or one whose stream is still rendered from the
    /// rolling tail, holds no rendered lines at all. Every non-parsing call
    /// releases whatever was allocated before, so the heap shrinks as soon as
    /// the block closes (c-2).
    pub fn update_markdown(&mut self) {
        if !self.show || self.streaming || self.text.is_empty() {
            self.lines.clear();
            return;
        }
        let mut theme = crate::markdown::MarkdownTheme::reasoning_themed(self.theme_mode);
        theme.max_width = self.max_width;
        self.lines = crate::markdown::render_markdown(&self.text, &theme);
    }

    /// Folds newly appended text into the rolling tail window.
    ///
    /// Text is consumed starting at [`Self::tail_consumed`], which tolerates
    /// deltas inserted directly into [`Self::text`] by event handlers.
    fn fold_tail(&mut self) {
        if self.tail_consumed >= self.text.len() {
            return;
        }
        let segment = self
            .text
            .get(self.tail_consumed..)
            .unwrap_or(self.text.as_str())
            .to_string();
        self.tail_consumed = self.text.len();

        let mut rest = segment.as_str();
        while let Some(idx) = rest.find('\n') {
            let (head, after) = rest.split_at(idx);
            self.pending.push_str(head);
            self.commit_pending_line();
            rest = &after[1..];
        }
        self.pending.push_str(rest);
    }

    fn commit_pending_line(&mut self) {
        let line = std::mem::take(&mut self.pending);
        self.tail.push(Self::tail_line(&line));
        if self.tail.len() > REASONING_TAIL_LINES {
            self.tail.remove(0);
        }
        self.newline_count += 1;
    }

    fn tail_line(line: &str) -> Line<'static> {
        let engine = current_engine();
        Line::from(Span::styled(
            line.to_string(),
            engine.fg(engine.palette().text_reasoning),
        ))
    }

    fn total_lines(&self) -> usize {
        // Counting the dropped prefix keeps the streaming counter monotonic: it
        // reports the length of the whole reasoning, not just of the window that
        // survived the cap.
        self.truncated_lines + self.newline_count + usize::from(!self.pending.is_empty())
    }

    /// True while a reasoning stream is active and not yet finalized.
    ///
    /// Text channels use this to settle and fold the reasoning block exactly
    /// once when the answer begins, so the lifecycle stays identical everywhere.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Flips the fold state and reconciles the rendered lines with it.
    ///
    /// Expanding is the only trigger of the Markdown parse; collapsing drops
    /// the rendered lines immediately instead of holding them until the block
    /// is opened again (c-2).
    pub fn toggle_fold(&mut self) {
        self.show = !self.show;
        self.update_markdown();
    }

    /// Notice that the oldest part of the transcript fell out of the window.
    ///
    /// It is rendered at the top of the expanded transcript so a reader never
    /// mistakes the retained window for the complete reasoning of the turn.
    fn truncation_marker(&self) -> Option<Line<'static>> {
        if self.truncated_lines == 0 {
            return None;
        }
        let engine = current_engine();
        Some(Line::from(Span::styled(
            crate::i18n::active_t("repl.thinking_truncated").replacen(
                "{}",
                &self.truncated_lines.to_string(),
                1,
            ),
            engine.fg(engine.palette().text_muted).bold(),
        )))
    }

    pub fn render_lines(&self) -> Vec<Line<'static>> {
        if self.text.is_empty() {
            return Vec::new();
        }

        let engine = current_engine();
        let palette = engine.palette();

        let mut lines = Vec::new();
        let reasoning_badge = Span::styled(
            " [REASONING] ",
            indicator_style(&engine, palette.button_active_bg),
        );

        if !self.show {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    format!("▶ {}", crate::i18n::active_t("repl.thinking_hidden")),
                    engine.fg(palette.text_muted).bold(),
                ),
            ]));
            return lines;
        }

        if self.streaming {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} {}",
                        crate::i18n::active_t("repl.thinking_streaming"),
                        crate::i18n::active_t("repl.thinking_lines").replacen(
                            "{}",
                            &self.total_lines().to_string(),
                            1
                        ),
                    ),
                    engine.fg(palette.status_info).bold(),
                ),
            ]));

            if let Some(marker) = self.truncation_marker() {
                lines.push(marker);
            }
            lines.extend(self.tail.iter().cloned());
            if !self.pending.is_empty() {
                lines.push(Self::tail_line(&self.pending));
            }
        } else {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    crate::i18n::active_t("repl.thinking"),
                    engine.fg(palette.status_info).bold(),
                ),
            ]));

            if let Some(marker) = self.truncation_marker() {
                lines.push(marker);
            }

            if !self.lines.is_empty() {
                lines.extend(self.lines.iter().cloned());
            } else {
                let text_style = engine.fg(palette.text_reasoning);
                for line in self.text.lines() {
                    lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(state: &ReasoningState) -> String {
        state
            .render_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.to_string())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn test_streaming_reasoning_badge_is_localized() {
        let mut state = ReasoningState::new();
        state.append_delta("first line\nsecond line\n");

        let rendered = rendered_text(&state);
        assert!(
            rendered.contains("Thinking (streaming)..."),
            "streaming badge must render the localized label, got: {rendered}"
        );
        assert!(
            rendered.contains("(2 lines)"),
            "streaming badge must render the localized line counter, got: {rendered}"
        );
    }

    #[test]
    fn test_hidden_reasoning_badge_is_localized() {
        let mut state = ReasoningState::new();
        state.append_delta("folded reasoning");
        state.show = false;

        let rendered = rendered_text(&state);
        assert!(
            rendered.contains("Thinking (hidden)"),
            "folded badge must render the localized label, got: {rendered}"
        );
    }

    #[test]
    fn test_settled_reasoning_badge_is_localized() {
        let mut state = ReasoningState::new();
        state.append_delta("settled reasoning");
        state.finalize();

        let collapsed = rendered_text(&state);
        assert!(
            collapsed.contains("Thinking (hidden)"),
            "a settled stream folds its transcript, got: {collapsed}"
        );

        state.toggle_fold();
        assert!(
            !state.lines.is_empty(),
            "expanding the block must parse the transcript"
        );
        let expanded = rendered_text(&state);
        assert!(
            expanded.contains("Thinking..."),
            "settled badge must render the localized label, got: {expanded}"
        );
    }

    #[test]
    fn transcript_window_keeps_the_newest_text() {
        let mut state = ReasoningState::new();
        let line = "reasoning line\n";
        let repeats = (REASONING_MAX_BYTES / line.len()) + 32;
        for _ in 0..repeats {
            state.append_delta(line);
        }

        assert!(
            state.text.len() <= REASONING_MAX_BYTES,
            "raw transcript must stay inside the cap, got {}",
            state.text.len()
        );
        assert!(
            state.truncated_lines > 0,
            "the oldest lines must be dropped once the cap is crossed"
        );
        assert!(
            state.text.ends_with(line),
            "the newest reasoning must survive the trim"
        );
    }

    #[test]
    fn transcript_window_never_splits_a_character() {
        // No newline anywhere: the whole transcript is one multi-byte line, so
        // the trim can only land on a character boundary.
        let mut state = ReasoningState::new();
        let chunk = "рассуждение без переводов строки, ".repeat(4);
        let repeats = (REASONING_MAX_BYTES / chunk.len()) + 8;
        for _ in 0..repeats {
            state.append_delta(&chunk);
        }

        assert!(
            state.text.len() <= REASONING_MAX_BYTES,
            "a newline-free transcript must still respect the cap, got {}",
            state.text.len()
        );
        assert!(
            state.text.chars().all(|c| !c.is_control() || c == '\n'),
            "the retained window must start on a character boundary"
        );
    }

    #[test]
    fn truncation_marker_reports_the_dropped_lines() {
        // A marker only ever accompanies a retained window, so the transcript
        // must be non-empty for the block to render at all.
        let mut state = ReasoningState::new();
        state.text.push_str("retained reasoning line");
        state.truncated_lines = 42;

        let rendered = rendered_text(&state);
        assert!(
            rendered.contains("42 earlier lines dropped"),
            "the marker must name how many lines were dropped, got: {rendered}"
        );
        assert!(
            rendered.contains("retained reasoning line"),
            "the retained window must follow the marker, got: {rendered}"
        );

        state.truncated_lines = 0;
        let clean = rendered_text(&state);
        assert!(
            !clean.contains("earlier lines dropped"),
            "an untrimmed transcript must not render the marker, got: {clean}"
        );
    }

    #[test]
    fn streaming_truncation_marker_reports_the_dropped_lines() {
        let mut state = ReasoningState::new();
        state.append_delta("live reasoning line\n");
        state.truncated_lines = 7;

        let rendered = rendered_text(&state);
        assert!(
            rendered.contains("7 earlier lines dropped"),
            "the streaming badge must also announce the dropped prefix, got: {rendered}"
        );
    }

    #[test]
    fn finalized_transcript_parses_nothing_until_expanded() {
        let mut state = ReasoningState::new();
        state.append_delta("**bold** reasoning line\n");
        state.finalize();

        assert!(!state.show, "a settled stream folds its transcript");
        assert!(
            state.lines.is_empty(),
            "a folded transcript must not hold parsed lines"
        );

        state.toggle_fold();
        assert!(state.show);
        assert!(
            !state.lines.is_empty(),
            "expanding the block must parse the transcript"
        );
    }

    #[test]
    fn collapsing_the_block_releases_the_parsed_lines() {
        let mut state = ReasoningState::new();
        state.append_delta("reasoning line\n");
        state.finalize();
        state.toggle_fold();
        assert!(!state.lines.is_empty());

        state.toggle_fold();
        assert!(!state.show);
        assert!(
            state.lines.is_empty(),
            "folding the block must drop the rendered lines"
        );
    }

    #[test]
    fn width_and_theme_updates_never_parse_a_folded_block() {
        let mut state = ReasoningState::new();
        state.append_delta("reasoning line\n");
        state.finalize();

        state.max_width = Some(80);
        state.update_markdown();
        assert!(
            state.lines.is_empty(),
            "a folded block must stay unparsed on a width change"
        );

        state.toggle_fold();
        assert!(!state.lines.is_empty(), "an expanded block parses");
        state.max_width = Some(40);
        state.update_markdown();
        assert!(
            !state.lines.is_empty(),
            "an expanded block re-parses under the new width"
        );
    }

    #[test]
    fn streaming_block_never_holds_parsed_lines() {
        let mut state = ReasoningState::new();
        state.append_delta("live reasoning line\n");
        // The block opens expanded, so the fold path is exercised by toggling
        // twice and returning the streaming block to the state it renders in.
        state.toggle_fold();
        state.toggle_fold();

        assert!(state.show);
        assert!(
            state.lines.is_empty(),
            "a running stream renders from its tail and parses nothing"
        );
    }

    #[test]
    fn tail_window_keeps_only_the_newest_lines() {
        let mut state = ReasoningState::new();
        let total = REASONING_TAIL_LINES * 3;
        for idx in 0..total {
            state.append_delta(&format!("line {idx}\n"));
        }

        assert_eq!(
            state.tail.len(),
            REASONING_TAIL_LINES,
            "the live window must never grow past its capacity"
        );

        let rendered = rendered_text(&state);
        assert!(
            !rendered.contains("line 0"),
            "the oldest line must fall out of the live window, got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("line {}", total - 1)),
            "the newest line must stay visible, got: {rendered}"
        );
        assert_eq!(
            state.total_lines(),
            total,
            "the counter reports the whole reasoning, not just the window"
        );
    }

    #[test]
    fn total_lines_stay_monotonic_across_a_window_trim() {
        let mut state = ReasoningState::new();
        let line = "reasoning line\n";
        let repeats = (REASONING_MAX_BYTES / line.len()) + 32;
        for _ in 0..repeats {
            state.append_delta(line);
        }

        assert!(state.truncated_lines > 0, "the cap must have trimmed");
        assert_eq!(
            state.total_lines(),
            repeats,
            "dropping the oldest lines must not reset the reasoning counter"
        );
    }

    #[test]
    fn truncation_offset_cuts_after_a_complete_line() {
        let text = "первая строка\nвторая строка\nтретья\n";
        let cut = ReasoningState::truncation_offset(text, text.len() / 3);

        assert_eq!(
            cut,
            "первая строка\n".len(),
            "the cut lands right after the first complete line"
        );
        assert!(text.is_char_boundary(cut));
    }

    #[test]
    fn truncation_offset_without_a_newline_stays_on_a_char_boundary() {
        let text = "рассуждение".repeat(4);
        let overflow = 7; // inside the second two-byte character
        let cut = ReasoningState::truncation_offset(&text, overflow);

        assert!(
            text.is_char_boundary(cut),
            "the cut must never split a character"
        );
        assert!(
            cut >= overflow,
            "the cut must land at or after the overflow, got {cut}"
        );
    }

    #[test]
    fn a_settled_transcript_finalizes_exactly_once() {
        let mut state = ReasoningState::new();
        state.append_delta("reasoning line\n");
        state.finalize();
        assert!(!state.is_streaming());

        // A second call must neither re-fold the block nor parse it.
        state.show = true;
        state.finalize();
        assert!(state.show);
        assert!(
            state.lines.is_empty(),
            "a settled transcript is never parsed by finalize"
        );
    }

    #[test]
    fn folding_cycles_release_the_lines_between_expansions() {
        let mut state = ReasoningState::new();
        state.append_delta("**bold** reasoning line\n");
        state.finalize();

        for _ in 0..3 {
            state.toggle_fold();
            assert!(state.show);
            assert!(
                !state.lines.is_empty(),
                "expanding must parse the transcript"
            );

            state.toggle_fold();
            assert!(!state.show);
            assert!(
                state.lines.is_empty(),
                "collapsing must release the parsed lines"
            );
        }
    }
}
