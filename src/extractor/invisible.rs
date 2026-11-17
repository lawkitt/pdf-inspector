//! What a page's invisible (render mode 3) text left out of extraction.

/// The invisible (`3 Tr`) text one content walk skipped: whether it skipped
/// any at all (what decides an `include_invisible` retry), and in how many
/// stretches, for callers that tell their reader what the text leaves out.
///
/// A stretch is invisible text with nothing extracted between its parts:
/// consecutive invisible shows count once, however many operators, text
/// objects or form XObjects they span, and anything the walk keeps between
/// them (visible text, an image placeholder) starts a new stretch. A show
/// of blank text (spaces only) is skipped but starts and counts nothing, as
/// hidden whitespace in a Word document does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InvisibleText {
    skipped: bool,
    stretches: u32,
    /// The walk's item count when the first and the latest counted show
    /// were skipped: a later show at the same count continues the stretch.
    first_at: Option<usize>,
    last_at: Option<usize>,
}

impl InvisibleText {
    /// Records one skipped invisible show, made when the walk had kept
    /// `items_len` items. `blank` marks a show whose bytes are only spaces.
    pub(crate) fn skip(&mut self, items_len: usize, blank: bool) {
        self.skipped = true;
        if blank {
            return;
        }
        if self.last_at != Some(items_len) {
            self.stretches += 1;
        }
        self.first_at.get_or_insert(items_len);
        self.last_at = Some(items_len);
    }

    /// Takes in what a nested walk (a form XObject) skipped, whose items are
    /// appended after the caller's first `base` items. A stretch the form
    /// opens before keeping anything continues one the caller left open.
    pub(crate) fn absorb(&mut self, nested: InvisibleText, base: usize) {
        self.skipped |= nested.skipped;
        if nested.stretches == 0 {
            return;
        }
        let continues = nested.first_at == Some(0) && self.last_at == Some(base);
        self.stretches += nested.stretches - u32::from(continues);
        if self.first_at.is_none() {
            self.first_at = nested.first_at.map(|at| at + base);
        }
        self.last_at = nested.last_at.map(|at| at + base);
    }

    /// Whether any non-empty invisible show was skipped.
    pub(crate) fn any(&self) -> bool {
        self.skipped
    }

    /// How many stretches of invisible text were left out.
    pub(crate) fn stretches(&self) -> u32 {
        self.stretches
    }
}

/// Whether a show operand's raw bytes are blank: spaces only, in a
/// single-byte encoding.
pub(crate) fn blank_show(raw: &[u8]) -> bool {
    raw.iter().all(|&byte| byte == b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_skips_are_one_stretch_and_kept_items_split_them() {
        let mut invisible = InvisibleText::default();
        assert!(!invisible.any());
        invisible.skip(0, false);
        invisible.skip(0, false);
        assert_eq!(invisible.stretches(), 1);
        // Visible text kept in between: a second stretch.
        invisible.skip(3, false);
        assert_eq!(invisible.stretches(), 2);
        // Blank shows are skipped but count nothing.
        invisible.skip(5, true);
        assert_eq!(invisible.stretches(), 2);
        assert!(invisible.any());
    }

    #[test]
    fn blank_only_skips_still_report_a_skip() {
        let mut invisible = InvisibleText::default();
        invisible.skip(0, true);
        assert!(invisible.any());
        assert_eq!(invisible.stretches(), 0);
    }

    #[test]
    fn a_form_continues_the_callers_open_stretch() {
        let mut page = InvisibleText::default();
        page.skip(2, false);
        let mut form = InvisibleText::default();
        form.skip(0, false);
        form.skip(1, false);
        // Appended at base 2, where the page's stretch is still open.
        page.absorb(form, 2);
        assert_eq!(page.stretches(), 2);
        // The page goes on hiding right after the form's last kept item.
        page.skip(3, false);
        assert_eq!(page.stretches(), 2);
        // A form appended after visible text opens a stretch of its own.
        let mut later = InvisibleText::default();
        later.skip(0, false);
        page.absorb(later, 7);
        assert_eq!(page.stretches(), 3);
    }
}
