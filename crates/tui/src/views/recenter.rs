//! Emacs `recenter-top-bottom` positioning shared by the cursor-driven views.
//!
//! Repeatedly pressing the recenter command on the same item walks a three-beat
//! cycle — center, top, bottom — and moving the cursor to another item restarts
//! the cycle from its first beat (Conventions.md §5.2). The component owns only
//! the cycle bookkeeping and the viewport arithmetic: a view keeps owning its
//! cursor, its layout offsets, and its scroll field, so the views share one
//! implementation of the positioning math without sharing their state.

/// Number of beats in the cycle: center, top, bottom.
const RECENTER_CYCLE_LEN: u8 = 3;

/// Three-beat Emacs `recenter-top-bottom` state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecenterState {
    /// Beat that the next press on the same item applies.
    cycle: u8,
    /// Item the current beat belongs to.
    last_index: Option<usize>,
}

impl RecenterState {
    /// Returns the viewport offset that reveals `selected_index` on the next beat.
    ///
    /// `start_offset..end_offset` is the item's screen range inside the document
    /// the caller has already laid out, `total_lines` is the document length, and
    /// `view_height` is the number of visible rows. The result always saturates
    /// into `[0, max_scroll]`, so the caller assigns it to its scroll field
    /// directly.
    pub fn recenter(
        &mut self,
        selected_index: usize,
        start_offset: usize,
        end_offset: usize,
        total_lines: usize,
        view_height: u16,
    ) -> u16 {
        // A different item always starts from the first beat, so the cycle never
        // carries over from the row the cursor just left.
        if self.last_index != Some(selected_index) {
            self.cycle = 0;
            self.last_index = Some(selected_index);
        } else {
            self.cycle = (self.cycle + 1) % RECENTER_CYCLE_LEN;
        }

        let height = view_height as usize;
        if height == 0 {
            return 0;
        }

        let max_scroll = total_lines.saturating_sub(height);
        let item_height = end_offset.saturating_sub(start_offset);

        let desired = match self.cycle {
            // Center. An item taller than the viewport cannot be centered, so its
            // top edge is aligned with the top of the window and the item is read
            // from its beginning instead of its middle.
            0 if item_height > height => start_offset,
            0 => start_offset
                .saturating_add(item_height / 2)
                .saturating_sub(height / 2),
            1 => start_offset,
            _ => end_offset.saturating_sub(height),
        };

        u16::try_from(desired.min(max_scroll)).unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW_HEIGHT: u16 = 10;

    #[test]
    fn first_press_on_a_new_item_centers_it() {
        let mut state = RecenterState::default();

        // A 4-line item inside a 10-line viewport of a 100-line document.
        assert_eq!(state.recenter(0, 40, 44, 100, VIEW_HEIGHT), 37);
    }

    #[test]
    fn repeated_presses_cycle_center_top_bottom() {
        let mut state = RecenterState::default();

        assert_eq!(state.recenter(3, 40, 44, 100, VIEW_HEIGHT), 37, "center");
        assert_eq!(state.recenter(3, 40, 44, 100, VIEW_HEIGHT), 40, "top");
        assert_eq!(state.recenter(3, 40, 44, 100, VIEW_HEIGHT), 34, "bottom");
        assert_eq!(
            state.recenter(3, 40, 44, 100, VIEW_HEIGHT),
            37,
            "the cycle wraps back to center"
        );
    }

    #[test]
    fn moving_to_another_item_restarts_the_cycle() {
        let mut state = RecenterState::default();

        assert_eq!(state.recenter(0, 40, 44, 100, VIEW_HEIGHT), 37, "center");
        assert_eq!(state.recenter(0, 40, 44, 100, VIEW_HEIGHT), 40, "top");
        // A different item starts over on the center beat.
        assert_eq!(state.recenter(1, 60, 64, 100, VIEW_HEIGHT), 57);
        // ... and its own cycle keeps advancing from there.
        assert_eq!(state.recenter(1, 60, 64, 100, VIEW_HEIGHT), 60);
    }

    #[test]
    fn offsets_saturate_into_the_scroll_range() {
        let mut state = RecenterState::default();

        // Centering the first item would scroll above the document start.
        assert_eq!(state.recenter(0, 0, 2, 100, VIEW_HEIGHT), 0);
        assert_eq!(state.recenter(0, 0, 2, 100, VIEW_HEIGHT), 0, "top beat");
        assert_eq!(state.recenter(0, 0, 2, 100, VIEW_HEIGHT), 0, "bottom beat");

        // The last item: aligning its top would scroll past the last full
        // screenful, so every beat saturates at the document end.
        let mut state = RecenterState::default();
        assert_eq!(state.recenter(9, 194, 200, 200, VIEW_HEIGHT), 190);
        assert_eq!(
            state.recenter(9, 194, 200, 200, VIEW_HEIGHT),
            190,
            "the top beat saturates at max_scroll"
        );
        assert_eq!(state.recenter(9, 194, 200, 200, VIEW_HEIGHT), 190);
    }

    #[test]
    fn item_taller_than_the_viewport_aligns_its_top_when_centering() {
        let mut state = RecenterState::default();

        // A 40-line item in a 10-line viewport: centering aligns the top edge so
        // the item is read from its beginning.
        assert_eq!(state.recenter(0, 50, 90, 200, VIEW_HEIGHT), 50, "center");
        assert_eq!(state.recenter(0, 50, 90, 200, VIEW_HEIGHT), 50, "top");
        assert_eq!(
            state.recenter(0, 50, 90, 200, VIEW_HEIGHT),
            80,
            "bottom aligns the last line with the last visible row"
        );
    }

    #[test]
    fn zero_height_viewport_has_no_positioning() {
        let mut state = RecenterState::default();

        assert_eq!(state.recenter(0, 40, 44, 100, 0), 0);
        assert_eq!(state.recenter(0, 40, 44, 100, 0), 0);
    }

    #[test]
    fn document_longer_than_the_viewport_unit_saturates() {
        let mut state = RecenterState::default();

        // The computed offset exceeds `u16`, so it saturates instead of wrapping
        // around and trapping the viewport near the document start.
        assert_eq!(
            state.recenter(0, 69_000, 69_003, 70_000, VIEW_HEIGHT),
            u16::MAX
        );
    }
}
