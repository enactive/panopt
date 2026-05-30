//! Soft-wrap helper for the body fields.
//!
//! `tui_textarea` 0.7 does not support visual line wrap - it tracks one row
//! per logical line and scrolls horizontally when a line outgrows the field.
//! For prose-shaped fields (todo / note bodies) that reads as "my text
//! disappeared off the right edge." This module re-renders the textarea's
//! buffer wrapped at the field width, and maps the textarea's logical cursor
//! position to a visual position so the host can place the terminal cursor on
//! the right cell.
//!
//! Wrapping is word-aware: a visual row breaks at the last whitespace that
//! fits, with the whitespace kept at the end of the upper row so every char
//! in the buffer still appears once in the visual grid (the cursor map relies
//! on that 1-to-1 correspondence). If a single token is wider than the field,
//! it falls back to a hard char-level break.
//!
//! Selection / highlight regions are not reflected - the body fields don't
//! surface them visually today.

use unicode_width::UnicodeWidthChar;

/// One logical line wrapped into visual rows that each fit in `width` cells.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wrapped {
    /// Visual rows, top to bottom, across every logical line.
    pub lines: Vec<String>,
    /// Where the logical cursor lands in the visual grid: `(row, col)`.
    /// `row` indexes [`Wrapped::lines`]; `col` is in cells from the left.
    pub cursor: (usize, usize),
    /// For each visual row, the logical row it came from and the char index
    /// inside that logical row at which this visual segment starts. Lets the
    /// inverse mapping ([`Wrapped::visual_to_logical`]) place clicks on the
    /// right character without re-running the wrap algorithm.
    pub segments: Vec<(usize, usize)>,
}

impl Wrapped {
    /// For a logical selection from `anchor` to `tip` (each a logical
    /// `(row, col)`), return the per-visual-row highlight ranges as
    /// `(visual_row, char_start_in_segment, char_end_in_segment)`. Char
    /// indices into the segment string - what the draw code splits on to
    /// build styled spans.
    ///
    /// Anchor and tip can arrive in either order (mouse-up after a backward
    /// drag); normalized inside. A zero-length selection (`anchor == tip`)
    /// returns an empty vector so the draw code can fast-path "no highlight".
    pub(crate) fn visual_selection_ranges(
        &self,
        anchor: (usize, usize),
        tip: (usize, usize),
    ) -> Vec<(usize, usize, usize)> {
        let (start, end) = if anchor <= tip {
            (anchor, tip)
        } else {
            (tip, anchor)
        };
        if start == end {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (vrow, seg) in self.lines.iter().enumerate() {
            let (lrow, char_start) = self.segments[vrow];
            if lrow < start.0 || lrow > end.0 {
                continue;
            }
            let seg_chars = seg.chars().count();
            let seg_char_end = char_start + seg_chars;
            // The selection's effective char range inside this segment,
            // clipped to the segment's own span.
            let sel_start = if lrow == start.0 {
                start.1.max(char_start)
            } else {
                char_start
            };
            let sel_end = if lrow == end.0 {
                end.1.min(seg_char_end)
            } else {
                seg_char_end
            };
            if sel_start >= sel_end {
                continue;
            }
            let from = sel_start - char_start;
            let to = sel_end - char_start;
            out.push((vrow, from, to));
        }
        out
    }

    /// Map a click on visual cell `(vrow, vcol)` back to the logical cursor
    /// position `(row, col)`. Past-end clicks clamp to the last visual row
    /// and end of its segment - the click-to-position gesture should never
    /// fail silently.
    pub(crate) fn visual_to_logical(&self, vrow: usize, vcol: usize) -> (usize, usize) {
        if self.segments.is_empty() {
            return (0, 0);
        }
        let vrow = vrow.min(self.segments.len() - 1);
        let (lrow, char_start) = self.segments[vrow];
        // Walk the segment's chars accumulating cell width until we reach
        // `vcol`; the count of chars consumed is the offset within the
        // segment, added to the segment's char_start.
        let seg = &self.lines[vrow];
        let mut col_in_seg = 0usize;
        let mut w_acc = 0usize;
        for c in seg.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w_acc + cw > vcol {
                break;
            }
            w_acc += cw;
            col_in_seg += 1;
        }
        (lrow, char_start + col_in_seg)
    }
}

/// Wrap `lines` at `width` cells per visual row, and locate the visual
/// position of the logical cursor `(row, col)`.
///
/// `width == 0` is treated as "no wrap": the input lines pass through and the
/// visual cursor matches the logical one in row, with col clamped.
pub(crate) fn wrap_for_display(lines: &[String], cursor: (usize, usize), width: usize) -> Wrapped {
    if width == 0 {
        let segments = (0..lines.len()).map(|r| (r, 0usize)).collect();
        return Wrapped {
            lines: lines.to_vec(),
            cursor,
            segments,
        };
    }
    let (clog_row, clog_col) = cursor;
    let mut out_lines: Vec<String> = Vec::new();
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut cvis: (usize, usize) = (0, 0);
    let mut found_cursor = false;

    for (lrow, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            // Empty logical line gets one empty visual row.
            if !found_cursor && lrow == clog_row {
                cvis = (out_lines.len(), 0);
                found_cursor = true;
            }
            out_lines.push(String::new());
            segments.push((lrow, 0));
            continue;
        }

        // Greedy word-wrap pass over this logical line. `i` is the start of the
        // segment under construction; `j` walks forward until adding the next
        // char would overflow `width`. When that happens, `last_break` is the
        // closest split point at or before `j` that lands just after a run of
        // whitespace - we break there so the whitespace stays at the end of
        // the upper row. With no break point available the segment splits at
        // `j` (char-level fallback for tokens wider than the field).
        let mut i = 0usize;
        while i < chars.len() {
            let mut visual_w = 0usize;
            let mut j = i;
            // The most recent char-index strictly after a whitespace inside
            // chars[i..j], or `None` if no whitespace has been seen yet.
            let mut last_break: Option<usize> = None;
            while j < chars.len() {
                let cw = UnicodeWidthChar::width(chars[j]).unwrap_or(0);
                if visual_w + cw > width {
                    break;
                }
                visual_w += cw;
                j += 1;
                if chars[j - 1].is_whitespace() && j > i {
                    last_break = Some(j);
                }
            }

            let seg_end = if j == chars.len() {
                // Everything from `i` fits in one segment - last row of the
                // line.
                j
            } else if let Some(b) = last_break {
                // Break just after the most recent whitespace.
                b
            } else {
                // No whitespace in this segment - split at width.
                j.max(i + 1)
            };

            let seg: String = chars[i..seg_end].iter().collect();

            // If the logical cursor falls in this segment, map it. End-of-line
            // (clog_col == chars.len()) lands on the final segment.
            if !found_cursor && lrow == clog_row {
                let in_segment = (i..seg_end).contains(&clog_col)
                    || (seg_end == chars.len() && clog_col == seg_end);
                if in_segment {
                    let col = visual_col_within(&seg, clog_col - i);
                    cvis = (out_lines.len(), col);
                    found_cursor = true;
                }
            }

            out_lines.push(seg);
            segments.push((lrow, i));
            i = seg_end;
        }
    }

    // Cursor past the end of the buffer (no lines or empty buffer): clamp.
    if !found_cursor {
        let r = out_lines.len().saturating_sub(1);
        cvis = (r, 0);
    }

    Wrapped {
        lines: out_lines,
        cursor: cvis,
        segments,
    }
}

/// The visual width, in cells, of the first `char_count` chars of `s`.
fn visual_col_within(s: &str, char_count: usize) -> usize {
    s.chars()
        .take(char_count)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wrap_is_an_identity_pass_through() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        let w = wrap_for_display(&lines, (1, 3), 0);
        assert_eq!(w.lines, lines);
        assert_eq!(w.cursor, (1, 3));
    }

    #[test]
    fn long_line_splits_into_visual_rows() {
        let lines = vec!["abcdefghij".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        assert_eq!(w.lines, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn cursor_at_start_of_logical_line_is_at_start_of_first_visual_row() {
        let lines = vec!["abcdefghij".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        assert_eq!(w.cursor, (0, 0));
    }

    #[test]
    fn cursor_inside_a_wrapped_segment() {
        let lines = vec!["abcdefghij".to_string()];
        // Logical col 6 -> in segment "efgh" (segment 1, segment_start_char = 4)
        // -> visual col 2.
        let w = wrap_for_display(&lines, (0, 6), 4);
        assert_eq!(w.cursor, (1, 2));
    }

    #[test]
    fn cursor_at_end_of_line_lands_after_last_visual_row() {
        let lines = vec!["abcdefghij".to_string()];
        // 10 chars total, width 4 -> segments "abcd","efgh","ij". End-of-line
        // cursor at col 10 sits at the end of "ij".
        let w = wrap_for_display(&lines, (0, 10), 4);
        assert_eq!(w.cursor, (2, 2));
    }

    #[test]
    fn cursor_in_a_later_logical_line() {
        let lines = vec!["abcdef".to_string(), "xy".to_string()];
        // Visual rows: "abcd", "ef", "xy". Logical cursor at (1, 1) -> visual (2, 1).
        let w = wrap_for_display(&lines, (1, 1), 4);
        assert_eq!(w.lines, vec!["abcd", "ef", "xy"]);
        assert_eq!(w.cursor, (2, 1));
    }

    #[test]
    fn empty_logical_line_emits_an_empty_visual_row() {
        let lines = vec!["".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        assert_eq!(w.lines, vec![""]);
        assert_eq!(w.cursor, (0, 0));
    }

    /// Word-wrap breaks at whitespace and keeps the trailing space at the
    /// end of the upper row so every character still appears in the visual.
    #[test]
    fn wraps_on_whitespace_keeping_the_space_on_the_upper_row() {
        let lines = vec!["hello world foo".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 8);
        assert_eq!(w.lines, vec!["hello ", "world ", "foo"]);
    }

    /// A single token longer than the field width falls back to char-level
    /// wrap so it always renders.
    #[test]
    fn long_word_falls_back_to_char_wrap() {
        let lines = vec!["supercalifragilistic".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 8);
        assert_eq!(w.lines, vec!["supercal", "ifragili", "stic"]);
    }

    /// A long token followed by a space breaks the token by chars but the
    /// trailing space still anchors the wrap of the next token.
    #[test]
    fn mixed_long_word_and_short_words() {
        let lines = vec!["abcdefghijk lmno pqr".to_string()];
        // chars=20, width=8.
        // i=0: visual fills with "abcdefgh"(8 chars), no whitespace yet -> char wrap at 8.
        //      push "abcdefgh", i=8.
        // i=8: chars[8..]="ijk lmno pqr". Walk: i=8,j=9,10,11(=' '),12,13,14,15,16 visual_w=8 stop at 16? Let me recount.
        //      chars[8]='i' w=1, chars[9]='j' w=2, chars[10]='k' w=3, chars[11]=' ' w=4 (last_break=12),
        //      chars[12]='l' w=5, chars[13]='m' w=6, chars[14]='n' w=7, chars[15]='o' w=8, chars[16]=' ' w=9 STOP.
        //      seg_end = last_break = 12. push "ijk ", i=12.
        // i=12: chars[12..]="lmno pqr". walk: 'l','m','n','o',' ','p','q','r' visual_w=1..=8 stops at next? 8 chars, all fit. j=20=chars.len(). push "lmno pqr".
        let w = wrap_for_display(&lines, (0, 0), 8);
        assert_eq!(w.lines, vec!["abcdefgh", "ijk ", "lmno pqr"]);
    }

    #[test]
    fn visual_to_logical_inverts_a_wrapped_segment() {
        let lines = vec!["hello world foo".to_string()];
        // Visual rows: "hello ", "world ", "foo".
        let w = wrap_for_display(&lines, (0, 0), 8);
        // Click on row 1, col 2 -> 'r' in "world " -> logical col 6 + 2 = 8.
        assert_eq!(w.visual_to_logical(1, 2), (0, 8));
        // Click on row 2, col 0 -> 'f' -> logical col 12.
        assert_eq!(w.visual_to_logical(2, 0), (0, 12));
    }

    #[test]
    fn visual_to_logical_clamps_past_end_clicks() {
        let lines = vec!["abc".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 8);
        // Click well past the end of the only visual row clamps to the
        // segment end - 3 chars.
        assert_eq!(w.visual_to_logical(0, 100), (0, 3));
        // Click on a visual row past the last clamps to the last row.
        assert_eq!(w.visual_to_logical(50, 1), (0, 1));
    }

    #[test]
    fn visual_selection_ranges_covers_a_wrapped_run() {
        let lines = vec!["hello world foo".to_string()];
        // Visual rows: "hello ", "world ", "foo".
        let w = wrap_for_display(&lines, (0, 0), 8);
        // Select "world foo" - chars 6..15 of the only logical line.
        let r = w.visual_selection_ranges((0, 6), (0, 15));
        // Row 1 ("world ") is fully covered: chars 0..6 within the segment.
        // Row 2 ("foo") is fully covered: chars 0..3.
        assert_eq!(r, vec![(1, 0, 6), (2, 0, 3)]);
    }

    #[test]
    fn visual_selection_ranges_normalizes_reversed_anchors() {
        let lines = vec!["abcdefghij".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        let forward = w.visual_selection_ranges((0, 2), (0, 6));
        let reverse = w.visual_selection_ranges((0, 6), (0, 2));
        assert_eq!(forward, reverse);
        assert!(!forward.is_empty());
    }

    #[test]
    fn visual_selection_ranges_empty_for_zero_length_selection() {
        let lines = vec!["abc".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        assert_eq!(w.visual_selection_ranges((0, 1), (0, 1)), Vec::new());
    }

    #[test]
    fn visual_to_logical_handles_empty_logical_line() {
        let lines = vec!["a".to_string(), "".to_string(), "b".to_string()];
        let w = wrap_for_display(&lines, (0, 0), 4);
        // Visual row 1 is the empty logical line - any vcol maps to (1, 0).
        assert_eq!(w.visual_to_logical(1, 0), (1, 0));
        assert_eq!(w.visual_to_logical(1, 10), (1, 0));
    }

    /// Word-wrap maps the cursor onto the row that actually holds the char
    /// under the cursor, not the geometric center of the logical line.
    #[test]
    fn cursor_lands_on_the_word_wrapped_row() {
        let lines = vec!["hello world foo".to_string()];
        // "hello " (0..6), "world " (6..12), "foo" (12..15).
        // Cursor at logical col 8 -> visual row 1, col 2 (in "world ").
        let w = wrap_for_display(&lines, (0, 8), 8);
        assert_eq!(w.cursor, (1, 2));
    }
}
