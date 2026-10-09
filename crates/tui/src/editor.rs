use std::path::{Path, PathBuf};

pub fn default_prompt_history_path() -> PathBuf {
    if Path::new(".tauqe").is_dir() || Path::new(".git").exists() {
        let p = Path::new(".tauqe");
        let _ = std::fs::create_dir_all(p);
        p.join("prompts.history")
    } else if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        let dir = PathBuf::from(state_home).join("tauqe");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("prompts.history")
    } else if let Ok(home) = std::env::var("HOME") {
        let dir = PathBuf::from(home).join(".local").join("state").join("tauqe");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("prompts.history")
    } else {
        PathBuf::from(".tauqe/prompts.history")
    }
}

#[derive(Debug, Clone, Default)]
pub struct InputEditor {
    pub text: String,
    pub cursor: usize,
    pub kill_ring: String,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub draft: String,
    pub history_path: Option<PathBuf>,
}

impl InputEditor {
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
        use std::io::Write;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::File::create(path) {
            for item in &self.history {
                if let Ok(serialized) = serde_json::to_string(item) {
                    let _ = writeln!(file, "{}", serialized);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.history_index = None;
        self.draft.clear();
    }

    pub fn clear_saving(&mut self) {
        if !self.text.is_empty() {
            self.kill_ring = self.text.clone();
            self.text.clear();
            self.cursor = 0;
            self.history_index = None;
            self.draft.clear();
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
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
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
        let (lines, cur) = crate::ui::develop::render::build_prompt_lines(self, width, false);
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

    pub fn kill_line(&mut self) {
        let line_end = self.current_line_end();
        if self.cursor == line_end {
            if self.cursor < self.text.len() {
                let removed = self.text.remove(self.cursor);
                self.kill_ring = removed.to_string();
            }
        } else {
            let killed: String = self.text.drain(self.cursor..line_end).collect();
            self.kill_ring = killed;
        }
    }

    pub fn kill_to_beginning_of_line(&mut self) {
        let line_start = self.current_line_start();
        if self.cursor > line_start {
            let killed: String = self.text.drain(line_start..self.cursor).collect();
            self.kill_ring = killed;
            self.cursor = line_start;
        } else if self.cursor > 0 {
            self.delete_backward();
        }
    }

    pub fn yank(&mut self) {
        if !self.kill_ring.is_empty() {
            let s = self.kill_ring.clone();
            self.insert_str(&s);
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
            self.kill_ring = killed;
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
        self.kill_ring = killed;
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
        self.kill_ring = killed;
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
}

#[cfg(test)]
mod tests {
    use super::InputEditor;

    #[test]
    fn paste_inserts_unicode_at_cursor_and_normalizes_line_endings() {
        let mut editor = InputEditor::default();
        editor.insert_str("началоконец");
        editor.cursor = "начало".len();
        editor.insert_paste("первая\r\nвторая\rтретья\n");

        assert_eq!(editor.get_text(), "началопервая\nвторая\nтретья\nконец");
        assert_eq!(editor.cursor, "началопервая\nвторая\nтретья\n".len());
    }

    #[test]
    fn large_paste_is_inserted_in_full() {
        let mut editor = InputEditor::default();
        let text = "Длинный промт ? [ ] u\n".repeat(10_000);
        editor.insert_paste(&text);

        assert_eq!(editor.get_text(), text.as_str());
        assert_eq!(editor.cursor, text.len());
    }

    #[test]
    fn prompt_history_navigation_and_draft_restoration() {
        let mut editor = InputEditor::default();
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
        let mut editor = InputEditor::default();
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

        let mut editor = InputEditor::default();
        editor.set_history_path(history_file.clone());

        editor.record_history("first simple prompt");
        editor.record_history("multiline\nprompt with \"quotes\" and <tags>");
        editor.record_history("third prompt");

        let mut reloaded = InputEditor::default();
        reloaded.set_history_path(history_file.clone());

        assert_eq!(reloaded.history.len(), 3);
        assert_eq!(reloaded.history[0], "first simple prompt");
        assert_eq!(reloaded.history[1], "multiline\nprompt with \"quotes\" and <tags>");
        assert_eq!(reloaded.history[2], "third prompt");

        let _ = std::fs::remove_file(history_file);
        let _ = std::fs::remove_dir(temp_dir);
    }
}
