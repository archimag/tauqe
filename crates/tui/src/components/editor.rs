use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use crate::command::editor::{EditorCommand, EDITOR_COMMAND_MAP};
use crate::config::{active_meta_modifier, active_primary_modifier, InputConfig};
use crate::theme::engine::current_engine;

/// Walks up from the current directory to the nearest ancestor that looks like
/// a Tauqe state root (contains `.tauqe` or `.git`). This keeps the prompt
/// history anchored to the repository root instead of the process working
/// directory, so launching the TUI from a subdirectory does not scatter or lose
/// the history. Returns `None` when no such ancestor exists.
fn find_state_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".tauqe").is_dir() || dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn default_prompt_history_path() -> PathBuf {
    if let Some(root) = find_state_root() {
        root.join(".tauqe").join("prompts.history")
    } else if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(state_home)
            .join("tauqe")
            .join("prompts.history")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("tauqe")
            .join("prompts.history")
    } else {
        PathBuf::from(".tauqe/prompts.history")
    }
}

/// Writes `contents` to `path` atomically (sibling temp file + rename) with
/// owner-only permissions on Unix.
///
/// The prompt history may contain secrets, so it must never be world-readable
/// and must never be left half-written if the process is interrupted mid-save.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = temp_sibling_path(path);

    let write_result = std::fs::File::create(&tmp).and_then(|mut file| {
        set_private_permissions(&file)?;
        file.write_all(contents)?;
        file.sync_all()
    });

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Sibling path used for the atomic write-then-rename, kept unique per process.
fn temp_sibling_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("prompts.history"));
    name.push(format!(".tmp.{}", std::process::id()));
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// Restricts a freshly written file to owner-only access on Unix. A no-op on
/// platforms without Unix file modes.
#[cfg(unix)]
fn set_private_permissions(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// Returns whether the key event's modifiers correspond to normal character typing:
/// either no modifiers, Shift only, or AltGr (Ctrl+Alt / Ctrl+Alt+Shift) on Windows/X11.
pub fn is_char_typing(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty()
        || modifiers == KeyModifiers::SHIFT
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        || modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// Builds the editor's modifier configuration from the process-global snapshot.
///
/// The shared [`EDITOR_COMMAND_MAP`] resolves `C-` / `M-` chords through the
/// developer's configured primary and meta modifiers, so the editor never
/// hardcodes `KeyModifiers::CONTROL` or `KeyModifiers::ALT`.
fn active_editor_input_config() -> InputConfig {
    InputConfig {
        primary_modifier: active_primary_modifier(),
        meta_modifier: active_meta_modifier(),
        ..InputConfig::default()
    }
}

/// Unified single/multiline text editor component.
///
/// Every text input surface (Develop prompt, modal discussion comments, squash
/// message, file search, onboarding fields) is backed by this component. It owns
/// the UTF-8 buffer, cursor navigation, the kill ring, prompt history with disk
/// persistence, keyboard handling and the visual line wrapping used to place the
/// terminal cursor.
#[derive(Debug, Clone, Default)]
pub struct TextEditor {
    pub text: String,
    pub cursor: usize,
    pub kill_ring: String,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub draft: String,
    pub history_path: Option<PathBuf>,
    /// Enables logical-line editing. When `true`, `Enter`, `C-j` and the `C-p` /
    /// `C-n` chords operate on lines; when `false` the editor behaves as a
    /// single-line field.
    pub multiline: bool,
}

impl TextEditor {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_history_path(&mut self, path: PathBuf) {
        self.load_history_from_file(&path);
        self.history_path = Some(path);
    }

    pub fn load_history_from_file(&mut self, path: &Path) {
        if let Ok(file) = std::fs::File::open(path) {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(file);
            let mut loaded = Vec::new();
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(prompt) = serde_json::from_str::<String>(trimmed) {
                    let prompt_trimmed = prompt.trim();
                    if !prompt_trimmed.is_empty()
                        && loaded.last().map(|s: &String| s.as_str()) != Some(prompt_trimmed)
                    {
                        loaded.push(prompt_trimmed.to_string());
                    }
                } else if !trimmed.is_empty()
                    && loaded.last().map(|s: &String| s.as_str()) != Some(trimmed)
                {
                    loaded.push(trimmed.to_string());
                }
            }
            if loaded.len() > 1000 {
                let start = loaded.len() - 1000;
                loaded = loaded.split_off(start);
            }
            self.history = loaded;
        }
    }

    pub fn save_history_to_file(&self, path: &Path) {
        let mut contents = String::new();
        for item in &self.history {
            if let Ok(serialized) = serde_json::to_string(item) {
                contents.push_str(&serialized);
                contents.push('\n');
            }
        }
        let _ = write_private_file(path, contents.as_bytes());
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_index = None;
        self.draft.clear();
    }

    pub fn clear_saving(&mut self) {
        if !self.text.is_empty() {
            let saved = self.text.clone();
            self.set_kill_ring(saved);
            self.text.clear();
            self.cursor = 0;
            self.history_index = None;
            self.draft.clear();
        }
    }

    fn set_kill_ring(&mut self, text: String) {
        if !text.is_empty() {
            crate::clipboard::copy_to_clipboard(&text);
            self.kill_ring = text;
        }
    }

    pub fn record_history(&mut self, prompt: &str) {
        let trimmed = prompt.trim();
        if !trimmed.is_empty() && self.history.last().map(|s| s.as_str()) != Some(trimmed) {
            self.history.push(trimmed.to_string());
            if let Some(ref path) = self.history_path {
                use std::io::Write;
                if self.history.len() > 1000 {
                    let start = self.history.len() - 1000;
                    self.history = self.history.split_off(start);
                    self.save_history_to_file(path);
                } else if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                    let mut options = std::fs::OpenOptions::new();
                    options.create(true).append(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    if let Ok(mut file) = options.open(path) {
                        let _ = set_private_permissions(&file);
                        if let Ok(serialized) = serde_json::to_string(trimmed) {
                            let _ = writeln!(file, "{}", serialized);
                        }
                    }
                }
            }
        }
        self.history_index = None;
        self.draft.clear();
    }

    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let new_idx = match self.history_index {
            None => {
                self.draft = self.text.clone();
                self.history.len().saturating_sub(1)
            }
            Some(0) => return false,
            Some(i) => i.saturating_sub(1),
        };
        self.history_index = Some(new_idx);
        if let Some(item) = self.history.get(new_idx) {
            self.text = item.clone();
            self.cursor = self.text.len();
            return true;
        }
        false
    }

    pub fn history_next(&mut self) -> bool {
        match self.history_index {
            None => false,
            Some(i) if i + 1 < self.history.len() => {
                let new_idx = i + 1;
                self.history_index = Some(new_idx);
                if let Some(item) = self.history.get(new_idx) {
                    self.text = item.clone();
                    self.cursor = self.text.len();
                }
                true
            }
            Some(_) => {
                self.history_index = None;
                self.text = self.draft.clone();
                self.cursor = self.text.len();
                true
            }
        }
    }

    pub fn get_text(&self) -> &str {
        &self.text
    }

    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            1
        } else {
            let count = self.text.split('\n').count();
            count.max(1)
        }
    }

    /// Computes the total visual line count and the visual line of the cursor
    /// given a specific content width (columns available inside the widget).
    pub fn visual_lines_and_cursor(&self, width: usize) -> (usize, usize) {
        let (lines, cur) = self.build_prompt_lines(width, false);
        (lines.len(), cur)
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for (i, c) in self.text.char_indices() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    pub fn get_lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            vec![""]
        } else {
            self.text.split('\n').collect()
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.history_index = None;
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.history_index = None;
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_paste(&mut self, text: &str) {
        // Clipboard text may use Windows or classic Mac line endings.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&normalized);
    }

    /// Inserts pasted text into a single-line field: surrounding whitespace is
    /// trimmed and embedded line breaks are collapsed to single spaces, so the
    /// Context file search and Onboarding inputs keep their single-line
    /// invariant without trailing newlines leaking into the buffer.
    pub fn insert_paste_single_line(&mut self, text: &str) {
        let normalized = text.trim().replace(['\n', '\r'], " ");
        self.insert_str(&normalized);
    }

    pub fn delete_backward(&mut self) {
        self.history_index = None;
        if let Some(prev_char) = self.text[..self.cursor].chars().next_back() {
            let new_cursor = self.cursor - prev_char.len_utf8();
            self.text.drain(new_cursor..self.cursor);
            self.cursor = new_cursor;
        }
    }

    pub fn delete_forward(&mut self) {
        self.history_index = None;
        if let Some(next_char) = self.text[self.cursor..].chars().next() {
            self.text
                .drain(self.cursor..self.cursor + next_char.len_utf8());
        }
    }

    pub fn move_backward(&mut self) {
        if let Some(prev_char) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= prev_char.len_utf8();
        }
    }

    pub fn move_forward(&mut self) {
        if let Some(next_char) = self.text[self.cursor..].chars().next() {
            self.cursor += next_char.len_utf8();
        }
    }

    pub fn current_line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0)
    }

    pub fn current_line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|idx| self.cursor + idx)
            .unwrap_or(self.text.len())
    }

    pub fn move_beginning_of_line(&mut self) {
        self.cursor = self.current_line_start();
    }

    pub fn move_end_of_line(&mut self) {
        self.cursor = self.current_line_end();
    }

    /// Moves the cursor to the very beginning of the buffer (`M-<`, `C-Home`).
    pub fn move_buffer_start(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the very end of the buffer (`M->`, `C-End`).
    pub fn move_buffer_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn kill_line(&mut self) {
        let line_end = self.current_line_end();
        if self.cursor == line_end {
            if self.cursor < self.text.len() {
                let removed = self.text.remove(self.cursor);
                self.set_kill_ring(removed.to_string());
            }
        } else {
            let killed: String = self.text.drain(self.cursor..line_end).collect();
            self.set_kill_ring(killed);
        }
    }

    pub fn kill_to_beginning_of_line(&mut self) {
        let line_start = self.current_line_start();
        if self.cursor > line_start {
            let killed: String = self.text.drain(line_start..self.cursor).collect();
            self.set_kill_ring(killed);
            self.cursor = line_start;
        } else if self.cursor > 0 {
            self.delete_backward();
        }
    }

    pub fn yank(&mut self) {
        // Consult the system clipboard (detecting the most recently updated
        // buffer between standard clipboard and Linux Primary Selection) and
        // fall back to the internal kill-ring when system clipboard is unavailable.
        let text = if let Some(clip) = crate::clipboard::get_from_clipboard() {
            if !clip.is_empty() {
                clip
            } else {
                self.kill_ring.clone()
            }
        } else {
            self.kill_ring.clone()
        };

        if !text.is_empty() {
            self.kill_ring = text.clone();
            self.insert_paste(&text);
        }
    }

    pub fn kill_word_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let after = &self.text[self.cursor..];
        let mut chars = after.char_indices();
        let mut first_word_idx = None;
        for (idx, c) in chars.by_ref() {
            if !c.is_whitespace() {
                first_word_idx = Some(idx);
                break;
            }
        }
        if first_word_idx.is_none() {
            let killed: String = self.text.drain(self.cursor..).collect();
            self.set_kill_ring(killed);
            return;
        }
        let mut end_idx = after.len();
        for (idx, c) in chars {
            if c.is_whitespace() {
                end_idx = idx;
                break;
            }
        }
        let killed: String = self
            .text
            .drain(self.cursor..self.cursor + end_idx)
            .collect();
        self.set_kill_ring(killed);
    }

    pub fn kill_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.text[..self.cursor];
        let mut char_indices: Vec<(usize, char)> = before.char_indices().collect();
        while let Some(&(_, c)) = char_indices.last() {
            if c.is_whitespace() {
                char_indices.pop();
            } else {
                break;
            }
        }
        while let Some(&(_, c)) = char_indices.last() {
            if !c.is_whitespace() {
                char_indices.pop();
            } else {
                break;
            }
        }
        let target_pos = char_indices
            .last()
            .map(|&(idx, c)| idx + c.len_utf8())
            .unwrap_or(0);
        let killed: String = self.text.drain(target_pos..self.cursor).collect();
        self.set_kill_ring(killed);
        self.cursor = target_pos;
    }

    pub fn move_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.text[..self.cursor];
        let mut char_indices: Vec<(usize, char)> = before.char_indices().collect();
        while let Some(&(_, c)) = char_indices.last() {
            if c.is_whitespace() {
                char_indices.pop();
            } else {
                break;
            }
        }
        while let Some(&(_, c)) = char_indices.last() {
            if !c.is_whitespace() {
                char_indices.pop();
            } else {
                break;
            }
        }
        self.cursor = char_indices
            .last()
            .map(|&(idx, c)| idx + c.len_utf8())
            .unwrap_or(0);
    }

    pub fn move_word_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let after = &self.text[self.cursor..];
        let mut chars = after.char_indices();
        let mut first_word_idx = None;
        for (idx, c) in chars.by_ref() {
            if !c.is_whitespace() {
                first_word_idx = Some(idx);
                break;
            }
        }
        if first_word_idx.is_none() {
            self.cursor = self.text.len();
            return;
        }
        let mut end_idx = after.len();
        for (idx, c) in chars {
            if c.is_whitespace() {
                end_idx = idx;
                break;
            }
        }
        self.cursor += end_idx;
    }

    pub fn move_line_up(&mut self) -> bool {
        let line_start = self.current_line_start();
        if line_start == 0 {
            return false;
        }
        let col = self.text[line_start..self.cursor].chars().count();
        let prev_line_end = line_start - 1;
        let prev_line_start = self.text[..prev_line_end]
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let prev_line_chars: Vec<usize> = self.text[prev_line_start..prev_line_end]
            .char_indices()
            .map(|(idx, _)| prev_line_start + idx)
            .collect();

        if col < prev_line_chars.len() {
            self.cursor = prev_line_chars[col];
        } else {
            self.cursor = prev_line_end;
        }
        true
    }

    pub fn move_line_down(&mut self) -> bool {
        let line_end = self.current_line_end();
        if line_end >= self.text.len() {
            return false;
        }
        let line_start = self.current_line_start();
        let col = self.text[line_start..self.cursor].chars().count();
        let next_line_start = line_end + 1;
        let next_line_end = self.text[next_line_start..]
            .find('\n')
            .map(|idx| next_line_start + idx)
            .unwrap_or(self.text.len());

        let next_line_chars: Vec<usize> = self.text[next_line_start..next_line_end]
            .char_indices()
            .map(|(idx, _)| next_line_start + idx)
            .collect();

        if col < next_line_chars.len() {
            self.cursor = next_line_chars[col];
        } else {
            self.cursor = next_line_end;
        }
        true
    }

    /// Dispatches a key event to the editor and returns `true` when the key was
    /// consumed as an editing or navigation command. Printable characters typed
    /// without a command modifier are always inserted unconditionally.
    ///
    /// Command chords resolve through the shared [`EDITOR_COMMAND_MAP`], the
    /// single source of truth for the editor's readline bindings, so this method
    /// never performs a raw `KeyModifiers::CONTROL` or `KeyModifiers::ALT` check.
    /// `is_cmd` carries the caller's precomputed primary-modifier state, while the
    /// meta modifier and the Shift-only chords (such as `Shift+Insert`) are
    /// matched from the map.
    pub fn handle_key(&mut self, key: KeyEvent, is_cmd: bool) -> bool {
        let input = active_editor_input_config();

        // Consult the map only for command chords, so a bare key never steals a
        // character from the buffer (Conventions §5). The map itself is the
        // authority on which physical modifier satisfies each abstract chord.
        // Raw Ctrl chords (the buffer-boundary `C-Home` / `C-End`) must reach
        // the map even when Ctrl is not the configured command modifier, so the
        // raw modifier joins the abstract ones in the gate.
        let command_chord = is_cmd
            || input.is_meta(key.modifiers)
            || key.modifiers.contains(KeyModifiers::SHIFT)
            || key.modifiers.contains(KeyModifiers::CONTROL);

        if command_chord {
            if let Some(spec) = EDITOR_COMMAND_MAP.resolve(key, &input, true) {
                return self.apply_editor_command(spec.action);
            }
        }

        match key.code {
            KeyCode::Char(c) if is_char_typing(key.modifiers) => {
                self.insert_char(c);
                true
            }
            KeyCode::Backspace => {
                self.delete_backward();
                true
            }
            KeyCode::Delete => {
                self.delete_forward();
                true
            }
            KeyCode::Left => {
                self.move_backward();
                true
            }
            KeyCode::Right => {
                self.move_forward();
                true
            }
            KeyCode::Home => {
                self.move_beginning_of_line();
                true
            }
            KeyCode::End => {
                self.move_end_of_line();
                true
            }
            KeyCode::Up if self.multiline => {
                self.move_line_up();
                true
            }
            KeyCode::Down if self.multiline => {
                self.move_line_down();
                true
            }
            KeyCode::Enter if self.multiline => {
                self.insert_char('\n');
                true
            }
            _ => false,
        }
    }

    /// Applies a resolved [`EditorCommand`] to the buffer.
    ///
    /// Returns `true` when the command was consumed as an editor command. The
    /// line-oriented operations report `false` in single-line mode, mirroring the
    /// historical behaviour where `C-p` / `C-n` / `C-j` were inert outside a
    /// multiline field.
    fn apply_editor_command(&mut self, command: EditorCommand) -> bool {
        match command {
            EditorCommand::MoveCharBackward => self.move_backward(),
            EditorCommand::MoveCharForward => self.move_forward(),
            EditorCommand::MoveWordBackward => self.move_word_backward(),
            EditorCommand::MoveWordForward => self.move_word_forward(),
            EditorCommand::MoveBeginningOfLine => self.move_beginning_of_line(),
            EditorCommand::MoveEndOfLine => self.move_end_of_line(),
            EditorCommand::MoveBufferStart => self.move_buffer_start(),
            EditorCommand::MoveBufferEnd => self.move_buffer_end(),
            EditorCommand::MoveLineUp => {
                if !self.multiline {
                    return false;
                }
                self.move_line_up();
            }
            EditorCommand::MoveLineDown => {
                if !self.multiline {
                    return false;
                }
                self.move_line_down();
            }
            EditorCommand::DeleteForward => self.delete_forward(),
            EditorCommand::InsertNewline => {
                if !self.multiline {
                    return false;
                }
                self.insert_char('\n');
            }
            EditorCommand::KillWordBackward => self.kill_word_backward(),
            EditorCommand::KillWordForward => self.kill_word_forward(),
            EditorCommand::KillToBeginningOfLine => self.kill_to_beginning_of_line(),
            EditorCommand::KillLine => self.kill_line(),
            EditorCommand::Yank => self.yank(),
        }
        true
    }

    /// Wraps the buffer into styled prompt lines and reports the visual line of
    /// the cursor. Wrapping and cursor placement are computed together so the
    /// terminal cursor never drifts from the rendered text.
    pub fn build_prompt_lines(
        &self,
        content_width: usize,
        is_busy: bool,
    ) -> (Vec<Line<'static>>, usize) {
        let content_width = content_width.max(10);
        let engine = current_engine();
        let palette = engine.palette();
        let prefix_style = if is_busy {
            engine.fg(palette.text_muted).bold()
        } else {
            engine.fg(palette.status_info).bold()
        };
        let cursor_style = engine.fg(palette.status_warning);

        if self.is_empty() {
            let line = Line::from(vec![
                Span::styled(" > ", prefix_style),
                Span::styled("█", cursor_style),
            ]);
            return (vec![line], 0);
        }

        let mut visual_lines: Vec<Line<'static>> = Vec::new();
        let mut cursor_visual_line: usize = 0;
        let mut byte_offset = 0;
        let logical_lines: Vec<&str> = self.text.split('\n').collect();

        for raw_line in logical_lines {
            let line_len = raw_line.len();
            let line_start = byte_offset;
            let line_end = byte_offset + line_len;
            byte_offset = line_end + 1;

            let cursor_in_line = self.cursor >= line_start && self.cursor <= line_end;
            let cursor_offset = if cursor_in_line {
                Some(self.cursor - line_start)
            } else {
                None
            };

            let mut current_spans: Vec<Span<'static>> = Vec::new();
            let mut current_width: usize = 0;

            let is_first_visual = visual_lines.is_empty();
            current_spans.push(Span::styled(
                if is_first_visual { " > " } else { "   " },
                prefix_style,
            ));
            current_width += 3;

            if raw_line.is_empty() {
                if cursor_offset == Some(0) {
                    current_spans.push(Span::styled("█", cursor_style));
                    cursor_visual_line = visual_lines.len();
                }
                visual_lines.push(Line::from(current_spans));
                continue;
            }

            let tokens = tokenize_with_offsets(raw_line);
            for (token_str, is_ws, token_start) in tokens {
                let token_end = token_start + token_str.len();
                let has_cursor = cursor_offset.is_some_and(|c| c >= token_start && c < token_end);

                let token_spans: Vec<Span<'static>> = match cursor_offset {
                    Some(c_off) if c_off >= token_start && c_off < token_end => {
                        let local_off = c_off - token_start;
                        let before = &token_str[..local_off];
                        let mut chars = token_str[local_off..].chars();
                        if let Some(ch) = chars.next() {
                            let ch_len = ch.len_utf8();
                            let after = &token_str[local_off + ch_len..];

                            let mut s = Vec::new();
                            if !before.is_empty() {
                                s.push(Span::raw(before.to_string()));
                            }
                            s.push(Span::styled(
                                ch.to_string(),
                                engine
                                    .bg(palette.text_primary)
                                    .fg(engine.color(palette.text_inverse))
                                    .bold(),
                            ));
                            if !after.is_empty() {
                                s.push(Span::raw(after.to_string()));
                            }
                            s
                        } else {
                            vec![Span::raw(token_str.to_string())]
                        }
                    }
                    _ => vec![Span::raw(token_str.to_string())],
                };

                let token_width: usize = token_spans.iter().map(|s| s.width()).sum();

                if is_ws {
                    if current_width + token_width <= content_width {
                        if has_cursor {
                            cursor_visual_line = visual_lines.len();
                        }
                        current_spans.extend(token_spans);
                        current_width += token_width;
                    } else {
                        // The whitespace does not fit on the current visual line.
                        // It is dropped at the wrap boundary, so when it carries
                        // the cursor, re-emit the cursor on the next line instead
                        // of letting it vanish from the screen.
                        if current_width > 3 {
                            visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                            current_spans.push(Span::styled("   ", prefix_style));
                            current_width = 3;
                        }
                        if has_cursor {
                            current_spans.push(Span::styled("█", cursor_style));
                            current_width += 1;
                            cursor_visual_line = visual_lines.len();
                        }
                    }
                } else if current_width + token_width <= content_width {
                    if has_cursor {
                        cursor_visual_line = visual_lines.len();
                    }
                    current_spans.extend(token_spans);
                    current_width += token_width;
                } else {
                    if current_width > 3 {
                        visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                        current_spans.push(Span::styled("   ", prefix_style));
                        current_width = 3;
                    }

                    if current_width + token_width <= content_width {
                        if has_cursor {
                            cursor_visual_line = visual_lines.len();
                        }
                        current_spans.extend(token_spans);
                        current_width += token_width;
                    } else {
                        for span in token_spans {
                            let style = span.style;
                            for ch in span.content.chars() {
                                let ch_str = ch.to_string();
                                let ch_w = Span::raw(&ch_str).width();
                                if current_width + ch_w > content_width && current_width > 3 {
                                    visual_lines
                                        .push(Line::from(std::mem::take(&mut current_spans)));
                                    current_spans.push(Span::styled("   ", prefix_style));
                                    current_width = 3;
                                }
                                if style != Style::default() {
                                    cursor_visual_line = visual_lines.len();
                                }
                                current_spans.push(Span::styled(ch_str, style));
                                current_width += ch_w;
                            }
                        }
                    }
                }
            }

            if cursor_offset == Some(line_len) {
                let cursor_span = Span::styled("█", cursor_style);
                let cursor_w = cursor_span.width();
                if current_width + cursor_w > content_width && current_width > 3 {
                    visual_lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_spans.push(Span::styled("   ", prefix_style));
                }
                cursor_visual_line = visual_lines.len();
                current_spans.push(cursor_span);
            }

            if !current_spans.is_empty() {
                visual_lines.push(Line::from(current_spans));
            }
        }

        if visual_lines.is_empty() {
            visual_lines.push(Line::from(vec![
                Span::styled(" > ", prefix_style),
                Span::styled("█", cursor_style),
            ]));
        }

        (visual_lines, cursor_visual_line)
    }
}

fn tokenize_with_offsets(s: &str) -> Vec<(&str, bool, usize)> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    if chars.is_empty() {
        return tokens;
    }
    let mut i = 0;
    while i < chars.len() {
        let is_ws = chars[i].1.is_whitespace();
        let start = chars[i].0;
        while i < chars.len() && chars[i].1.is_whitespace() == is_ws {
            i += 1;
        }
        let end = if i < chars.len() { chars[i].0 } else { s.len() };
        tokens.push((&s[start..end], is_ws, start));
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn paste_inserts_unicode_at_cursor_and_normalizes_line_endings() {
        let mut editor = TextEditor::default();
        editor.insert_str("началоконец");
        editor.cursor = "начало".len();
        editor.insert_paste("первая\r\nвторая\rтретья\n");

        assert_eq!(editor.get_text(), "началопервая\nвторая\nтретья\nконец");
        assert_eq!(editor.cursor, "началопервая\nвторая\nтретья\n".len());
    }

    #[test]
    fn large_paste_is_inserted_in_full() {
        let mut editor = TextEditor::default();
        let text = "Длинный промт ? [ ] u\n".repeat(10_000);
        editor.insert_paste(&text);

        assert_eq!(editor.get_text(), text.as_str());
        assert_eq!(editor.cursor, text.len());
    }

    #[test]
    fn prompt_history_navigation_and_draft_restoration() {
        let mut editor = TextEditor::default();
        editor.record_history("first prompt");
        editor.record_history("second prompt");

        editor.insert_str("unfinished draft");
        assert!(editor.history_prev());
        assert_eq!(editor.get_text(), "second prompt");

        assert!(editor.history_prev());
        assert_eq!(editor.get_text(), "first prompt");

        assert!(!editor.history_prev()); // At oldest
        assert_eq!(editor.get_text(), "first prompt");

        assert!(editor.history_next());
        assert_eq!(editor.get_text(), "second prompt");

        assert!(editor.history_next()); // Back to draft
        assert_eq!(editor.get_text(), "unfinished draft");
    }

    #[test]
    fn clear_saving_preserves_kill_ring_and_yank_restores_it() {
        let mut editor = TextEditor::default();
        editor.insert_str("important multiline\nprompt to keep");
        editor.clear_saving();

        assert!(editor.is_empty());
        assert_eq!(editor.kill_ring, "important multiline\nprompt to keep");

        editor.yank();
        assert_eq!(editor.get_text(), "important multiline\nprompt to keep");
    }

    #[test]
    fn prompt_history_persists_multiline_to_disk_and_reloads() {
        let temp_dir = std::env::temp_dir().join(format!("tauqe_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let history_file = temp_dir.join("prompts.history");

        let mut editor = TextEditor::default();
        editor.set_history_path(history_file.clone());

        editor.record_history("first simple prompt");
        editor.record_history("multiline\nprompt with \"quotes\" and <tags>");
        editor.record_history("third prompt");

        let mut reloaded = TextEditor::default();
        reloaded.set_history_path(history_file.clone());

        assert_eq!(reloaded.history.len(), 3);
        assert_eq!(reloaded.history[0], "first simple prompt");
        assert_eq!(
            reloaded.history[1],
            "multiline\nprompt with \"quotes\" and <tags>"
        );
        assert_eq!(reloaded.history[2], "third prompt");

        let _ = std::fs::remove_file(history_file);
        let _ = std::fs::remove_dir(temp_dir);
    }

    #[test]
    fn shift_insert_yanks_into_editor() {
        let mut editor = TextEditor::default();
        editor.set_kill_ring("pasted text".to_string());
        let key = KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT);
        assert!(editor.handle_key(key, false));
        assert_eq!(editor.get_text(), "pasted text");
    }

    #[test]
    fn kill_then_yank_uses_internal_kill_ring() {
        let mut editor = TextEditor::default();
        editor.insert_str("alpha beta");
        editor.kill_word_backward();
        assert_eq!(editor.get_text(), "alpha ");
        assert_eq!(editor.kill_ring, "beta");

        editor.yank();
        assert_eq!(editor.get_text(), "alpha beta");
    }

    #[cfg(unix)]
    #[test]
    fn save_history_to_file_writes_private_atomic_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = std::env::temp_dir().join(format!("tauqe_atomic_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let history_file = temp_dir.join("prompts.history");

        let editor = TextEditor {
            history: vec!["secret prompt".to_string()],
            ..Default::default()
        };
        editor.save_history_to_file(&history_file);

        let mode = std::fs::metadata(&history_file)
            .expect("history file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(std::fs::read_to_string(&history_file)
            .expect("history readable")
            .contains("secret prompt"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn record_history_creates_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = std::env::temp_dir().join(format!("tauqe_perm_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let history_file = temp_dir.join("prompts.history");

        let mut editor = TextEditor::default();
        editor.set_history_path(history_file.clone());
        editor.record_history("first prompt");

        let mode = std::fs::metadata(&history_file)
            .expect("history file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn cursor_on_wrapped_space_stays_visible_on_next_line() {
        let mut editor = TextEditor::default();
        editor.insert_str("aaaaaaa b");
        editor.cursor = 7; // cursor sits on the space at the wrap boundary

        let (lines, cursor_line) = editor.build_prompt_lines(10, false);

        assert_eq!(lines.len(), 2);
        assert_eq!(cursor_line, 1);
    }

    #[test]
    fn command_chords_resolve_through_the_editor_map() {
        let mut editor = TextEditor::default();
        editor.insert_str("alpha beta");
        assert_eq!(editor.cursor, "alpha beta".len());

        // `M-b` (Meta word-backward) is resolved from the shared editor command
        // map rather than from a hardcoded Alt modifier check.
        let meta_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT);
        assert!(editor.handle_key(meta_b, false));
        assert_eq!(editor.cursor, "alpha ".len());
    }

    #[test]
    fn meta_backspace_kills_word_through_the_map() {
        let mut editor = TextEditor::default();
        editor.insert_str("alpha beta");

        let meta_backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert!(editor.handle_key(meta_backspace, false));
        assert_eq!(editor.get_text(), "alpha ");
    }

    #[test]
    fn buffer_boundary_chords_move_the_cursor_to_the_edges() {
        let mut editor = TextEditor {
            multiline: true,
            ..Default::default()
        };
        editor.insert_str("first line\nsecond line");

        // `M-<` (Alt+Shift+,) jumps to the beginning of the buffer.
        let meta_less = KeyEvent::new(KeyCode::Char('<'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert!(editor.handle_key(meta_less, false));
        assert_eq!(editor.cursor, 0);

        // `C-End` jumps to the end of the buffer, not of the current line.
        let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);
        assert!(editor.handle_key(ctrl_end, false));
        assert_eq!(editor.cursor, editor.text.len());

        // `M->` and `C-Home` mirror the same boundaries in the other direction.
        let meta_greater = KeyEvent::new(KeyCode::Char('>'), KeyModifiers::ALT);
        assert!(editor.handle_key(meta_greater, false));
        assert_eq!(editor.cursor, editor.text.len());

        let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
        assert!(editor.handle_key(ctrl_home, false));
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn command_newline_is_inert_in_single_line_mode() {
        let mut editor = TextEditor {
            multiline: false,
            ..Default::default()
        };
        editor.insert_str("line");

        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(!editor.handle_key(ctrl_j, true));
        assert_eq!(editor.get_text(), "line");
    }
}
