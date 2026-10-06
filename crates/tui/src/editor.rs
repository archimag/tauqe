#[derive(Debug, Clone, Default)]
pub struct InputEditor {
    pub text: String,
    pub cursor: usize,
    pub kill_ring: String,
}

impl InputEditor {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
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
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_paste(&mut self, text: &str) {
        // Clipboard text may use Windows or classic Mac line endings.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&normalized);
    }

    pub fn delete_backward(&mut self) {
        if self.cursor > 0 {
            let prev_char = self.text[..self.cursor].chars().next_back().unwrap();
            let prev_len = prev_char.len_utf8();
            let new_cursor = self.cursor - prev_len;
            self.text.drain(new_cursor..self.cursor);
            self.cursor = new_cursor;
        }
    }

    pub fn delete_forward(&mut self) {
        if self.cursor < self.text.len() {
            let next_char = self.text[self.cursor..].chars().next().unwrap();
            let next_len = next_char.len_utf8();
            self.text.drain(self.cursor..self.cursor + next_len);
        }
    }

    pub fn move_backward(&mut self) {
        if self.cursor > 0 {
            let prev_char = self.text[..self.cursor].chars().next_back().unwrap();
            self.cursor -= prev_char.len_utf8();
        }
    }

    pub fn move_forward(&mut self) {
        if self.cursor < self.text.len() {
            let next_char = self.text[self.cursor..].chars().next().unwrap();
            let next_char_len = next_char.len_utf8();
            self.cursor += next_char_len;
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
}
