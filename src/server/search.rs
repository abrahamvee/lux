//! Plain-text scrollback search: where a query occurs and which occurrence
//! to jump to next.

use std::ops::Range;

/// One occurrence, on a stable row so it survives scrollback trimming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub row: isize,
    pub cols: Range<usize>,
}

/// Column spans of `query` in one row, given its cells as
/// `(column, width, text)` in column order. Columns no cell covers count
/// as blanks.
pub fn spans<'a>(
    cells: impl IntoIterator<Item = (usize, usize, &'a str)>,
    query: &str,
) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut text = String::new();
    // Each cell's byte offset in `text` and its column span.
    let mut at: Vec<(usize, Range<usize>)> = Vec::new();
    let mut next_col = 0;
    for (col, width, s) in cells {
        for gap in next_col..col {
            at.push((text.len(), gap..gap + 1));
            text.push(' ');
        }
        let width = width.max(1);
        at.push((text.len(), col..col + width));
        text.push_str(s);
        next_col = col + width;
    }
    text.match_indices(query)
        .map(|(start, found)| {
            let first = at.partition_point(|(b, _)| *b <= start) - 1;
            let last = at.partition_point(|(b, _)| *b < start + found.len()) - 1;
            at[first].1.start..at[last].1.end
        })
        .collect()
}

/// The nearest match on a row above `row`, for matches in row order.
pub fn above(matches: &[Match], row: isize) -> Option<&Match> {
    matches.iter().rev().find(|m| m.row < row)
}

/// The nearest match on a row below `row`, for matches in row order.
pub fn below(matches: &[Match], row: isize) -> Option<&Match> {
    matches.iter().find(|m| m.row > row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrow(s: &str) -> Vec<(usize, usize, &str)> {
        s.char_indices()
            .map(|(i, _)| (i, 1, &s[i..i + 1]))
            .collect()
    }

    #[test]
    fn every_occurrence_in_a_row_is_found() {
        assert_eq!(spans(narrow("foo bar foo"), "foo"), vec![0..3, 8..11]);
        assert_eq!(spans(narrow("aaa"), "aa"), vec![0..2]);
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert_eq!(spans(narrow("Foo foo"), "foo"), vec![4..7]);
    }

    #[test]
    fn empty_query_and_missing_text_match_nothing() {
        assert!(spans(narrow("foo"), "").is_empty());
        assert!(spans(narrow("foo"), "bar").is_empty());
        assert!(spans(Vec::new(), "x").is_empty());
    }

    #[test]
    fn wide_cells_span_both_columns() {
        let cells = vec![(0, 1, "a"), (1, 2, "日"), (3, 1, "b")];
        assert_eq!(spans(cells, "日b"), vec![1..4]);
    }

    #[test]
    fn uncovered_columns_read_as_blanks() {
        let cells = vec![(0, 1, "a"), (3, 1, "b")];
        assert_eq!(spans(cells.clone(), "a  b"), vec![0..4]);
        assert!(spans(cells, "ab").is_empty());
    }

    #[test]
    fn jumps_pick_the_nearest_row_in_each_direction() {
        let m = |row| Match { row, cols: 0..1 };
        let matches = vec![m(2), m(5), m(5), m(9)];
        assert_eq!(above(&matches, 9), Some(&m(5)));
        assert_eq!(above(&matches, 5), Some(&m(2)));
        assert_eq!(above(&matches, 2), None);
        assert_eq!(below(&matches, 2), Some(&m(5)));
        assert_eq!(below(&matches, 5), Some(&m(9)));
        assert_eq!(below(&matches, 9), None);
        assert_eq!(above(&[], 3), None);
    }
}
