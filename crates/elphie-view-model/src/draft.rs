#[derive(Default)]
pub struct Draft {
    pub(super) text: String,
    pub(super) cursor: usize,
}

impl Draft {
    pub(super) fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn parts(&self) -> (&str, &str) {
        self.text.split_at(self.cursor)
    }

    pub(super) fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
    }

    pub(super) fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
    }

    pub(super) fn up(&mut self) {
        let Some((earlier, column)) = self.text[..self.cursor].rsplit_once('\n') else {
            return;
        };
        let line = earlier.rsplit('\n').next().unwrap_or_default();
        self.cursor = earlier.len() - line.len() + byte_at_column(line, column.chars().count());
    }

    pub(super) fn down(&mut self) {
        let Some((rest, later)) = self.text[self.cursor..].split_once('\n') else {
            return;
        };
        let column = self.text[..self.cursor]
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count();
        let line = later.split('\n').next().unwrap_or_default();
        self.cursor += rest.len() + 1 + byte_at_column(line, column);
    }

    pub(super) fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }

    pub(super) fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }
}

fn byte_at_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(i, _)| i)
}
