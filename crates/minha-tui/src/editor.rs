use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VisualLine {
    pub(crate) text: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) width: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EditorLayout {
    pub(crate) lines: Vec<VisualLine>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_column: usize,
}

impl EditorLayout {
    pub(crate) fn new(text: &str, cursor: usize, inner_width: usize) -> Self {
        let first_width = inner_width.saturating_sub(2).max(1);
        let continuation_width = inner_width.max(1);
        let mut lines = vec![VisualLine {
            text: String::new(),
            start: 0,
            end: 0,
            width: 0,
        }];
        let mut row = 0;
        let mut column = 0;
        let mut cursor_position = None;

        for (index, grapheme) in text.grapheme_indices(true) {
            if index == cursor {
                cursor_position = Some((row, column));
            }
            if grapheme == "\n" {
                lines[row].end = index;
                if lines[row].text.is_empty() && lines[row].start == index && row > 0 {
                    lines[row].start = index + grapheme.len();
                    lines[row].end = index + grapheme.len();
                    continue;
                }
                row += 1;
                column = 0;
                lines.push(VisualLine {
                    text: String::new(),
                    start: index + grapheme.len(),
                    end: index + grapheme.len(),
                    width: 0,
                });
                continue;
            }
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            let capacity = if row == 0 { first_width } else { continuation_width };
            if column > 0 && column + grapheme_width > capacity {
                lines[row].end = index;
                row += 1;
                column = 0;
                lines.push(VisualLine {
                    text: String::new(),
                    start: index,
                    end: index,
                    width: 0,
                });
                if index == cursor {
                    cursor_position = Some((row, column));
                }
            }
            lines[row].text.push_str(grapheme);
            column += grapheme_width;
            lines[row].end = index + grapheme.len();
            lines[row].width = column;

            let capacity = if row == 0 { first_width } else { continuation_width };
            if column >= capacity {
                row += 1;
                column = 0;
                lines.push(VisualLine {
                    text: String::new(),
                    start: index + grapheme.len(),
                    end: index + grapheme.len(),
                    width: 0,
                });
            }
        }
        if cursor == text.len() {
            cursor_position = Some((row, column));
        }
        let (cursor_row, cursor_column) = cursor_position.unwrap_or((row, column));
        Self {
            lines,
            cursor_row,
            cursor_column,
        }
    }

    pub(crate) fn byte_at_column(&self, text: &str, row: usize, target: usize) -> usize {
        let Some(line) = self.lines.get(row) else {
            return text.len();
        };
        let mut column = 0;
        for (offset, grapheme) in text[line.start..line.end].grapheme_indices(true) {
            let width = UnicodeWidthStr::width(grapheme).max(1);
            if column + width > target {
                return line.start + offset;
            }
            column += width;
        }
        line.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_prefix_only_reduces_the_first_visual_line() {
        let layout = EditorLayout::new("abcdefghi", 9, 6);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["abcd", "efghi"]
        );
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 5));
    }

    #[test]
    fn layout_tracks_graphemes_newlines_and_exact_width_cursor() {
        let layout = EditorLayout::new("a界\ne\u{301}x", "a界\ne\u{301}x".len(), 5);
        assert_eq!(layout.lines[0].text, "a界");
        assert_eq!(layout.lines[1].text, "e\u{301}x");
        assert_eq!((layout.cursor_row, layout.cursor_column), (1, 2));

        let exact = EditorLayout::new("abcd", 4, 6);
        assert_eq!((exact.cursor_row, exact.cursor_column), (1, 0));
    }
}
