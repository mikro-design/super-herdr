//! A rendered terminal screen, for clients that cannot carry an emulator.
//!
//! The daemon relays terminal output as opaque bytes, and a client that owns a
//! VT parser renders them itself; that is what the TUI does and it stays the
//! primary path. A browser is the case that path does not serve: giving a
//! read-only viewer its own emulator means shipping a megabyte of third-party
//! JavaScript inside a signed binary, in the component most exposed to a
//! network, for input handling such a viewer does not want.
//!
//! So the parse happens where a parser already exists. This turns a screen into
//! runs of identically styled cells, which a client paints without interpreting
//! an escape sequence. It is deliberately a second representation rather than a
//! replacement: nothing here changes what a frame subscriber receives.

use serde::{Deserialize, Serialize};

/// A colour exactly as the terminal expressed it.
///
/// Indexed colours stay indexed rather than being resolved to RGB here, because
/// the palette belongs to whoever is displaying them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Color {
    Default,
    Indexed { index: u8 },
    Rgb { red: u8, green: u8, blue: u8 },
}

impl Color {
    /// `None` when the terminal expressed no preference.
    const fn specified(self) -> Option<Self> {
        match self {
            Self::Default => None,
            other => Some(other),
        }
    }
}

impl From<vt100::Color> for Color {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed { index },
            vt100::Color::Rgb(red, green, blue) => Self::Rgb { red, green, blue },
        }
    }
}

/// How a run of cells is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Style {
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub foreground: Option<Color>,
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub background: Option<Color>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    /// Inverse video that swapping colours cannot express.
    ///
    /// Inverse is resolved here by exchanging the two colours, which says
    /// nothing at all when both are the terminal's defaults: swapping `Default`
    /// with `Default` yields `Default`, and the highlight disappears into a
    /// plain run. That is not a rare corner — `\x1b[7m` with no colours set is
    /// the usual way selections, status lines and highlighted rows are drawn.
    ///
    /// Resolving it here would mean naming concrete colours, which belong to
    /// whoever paints them. So the one case that cannot be expressed as a swap
    /// travels as itself: a viewer exchanging its own two default colours needs
    /// no palette knowledge either.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inverse: bool,
}

fn is_default_color(color: &Option<Color>) -> bool {
    matches!(color, None | Some(Color::Default))
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// A stretch of cells sharing one style.
///
/// Runs rather than cells because a terminal is mostly repetition: a screen of
/// eighty by twenty-four is nineteen hundred cells and usually a few dozen runs,
/// and the difference is what a phone pays for on every frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    #[serde(default, skip_serializing_if = "is_plain")]
    pub style: Style,
}

fn is_plain(style: &Style) -> bool {
    *style == Style::default()
}

/// One screen, rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Which update this is. A diff names the sequence it follows, so a client
    /// that missed one refuses to paint rather than carrying a corrupted grid
    /// indefinitely — a dropped update is otherwise wrong forever and silent.
    pub sequence: u64,
    pub width: u16,
    pub height: u16,
    pub rows: Vec<Vec<Run>>,
    /// Absent when the terminal hides its cursor, so a viewer never draws one
    /// the program asked to remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub column: u16,
}

impl Snapshot {
    /// Render the visible screen as update `sequence`.
    pub fn of(screen: &vt100::Screen, sequence: u64) -> Self {
        let (height, width) = screen.size();
        let rows = (0..height)
            .map(|row| render_row(screen, row, width))
            .collect();
        let cursor = if screen.hide_cursor() {
            None
        } else {
            let (row, column) = screen.cursor_position();
            Some(Cursor { row, column })
        };
        Self {
            sequence,
            width,
            height,
            rows,
            cursor,
        }
    }

    /// Bring this screen up to date, or refuse.
    ///
    /// A diff is only meaningful against the exact update it was computed
    /// from. Applying one to anything else would paint a grid that is wrong in
    /// ways nothing later corrects, so the mismatch is an error and the caller
    /// asks for a full snapshot instead.
    pub fn apply(&mut self, diff: &Diff) -> Result<(), StaleScreen> {
        if diff.follows != self.sequence {
            return Err(StaleScreen {
                held: self.sequence,
                expected: diff.follows,
            });
        }
        for update in &diff.rows {
            let Some(row) = self.rows.get_mut(usize::from(update.row)) else {
                return Err(StaleScreen {
                    held: self.sequence,
                    expected: diff.follows,
                });
            };
            row.clone_from(&update.runs);
        }
        self.cursor = diff.cursor;
        self.sequence = diff.sequence;
        Ok(())
    }

    /// The rows that differ from an earlier snapshot of the same screen.
    ///
    /// A resize changes every row by definition, so it reports all of them
    /// rather than comparing rows that no longer describe the same cells.
    pub fn changed_rows(&self, previous: &Self) -> Vec<u16> {
        if self.width != previous.width || self.height != previous.height {
            return (0..self.height).collect();
        }
        self.rows
            .iter()
            .zip(previous.rows.iter())
            .enumerate()
            .filter(|(_, (current, earlier))| current != earlier)
            .map(|(index, _)| index as u16)
            .collect()
    }
}

/// A client holding the wrong update asked to apply a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleScreen {
    pub held: u64,
    pub expected: u64,
}

impl std::fmt::Display for StaleScreen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "screen is at update {} but the diff follows {}; ask for a snapshot",
            self.held, self.expected
        )
    }
}

impl std::error::Error for StaleScreen {}

/// The rows of one screen that a later screen replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdate {
    pub row: u16,
    pub runs: Vec<Run>,
}

/// What changed between two updates of the same screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// The update this applies to.
    pub follows: u64,
    /// The update it produces.
    pub sequence: u64,
    pub rows: Vec<RowUpdate>,
    /// Always stated rather than omitted when unchanged, because a cursor that
    /// was hidden and one that simply did not move are different screens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}

impl Diff {
    /// What a client holding `previous` needs in order to hold `current`.
    ///
    /// `None` when the screen was resized: every row then describes different
    /// cells, and a full snapshot is the only honest answer.
    pub fn between(previous: &Snapshot, current: &Snapshot) -> Option<Self> {
        if previous.width != current.width || previous.height != current.height {
            return None;
        }
        let rows = current
            .changed_rows(previous)
            .into_iter()
            .map(|row| RowUpdate {
                row,
                runs: current.rows[usize::from(row)].clone(),
            })
            .collect();
        Some(Self {
            follows: previous.sequence,
            sequence: current.sequence,
            rows,
            cursor: current.cursor,
        })
    }
}

fn render_row(screen: &vt100::Screen, row: u16, width: u16) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for column in 0..width {
        let Some(cell) = screen.cell(row, column) else {
            continue;
        };
        // A wide character occupies two columns and reports its contents once;
        // emitting the continuation would duplicate the glyph.
        if cell.is_wide_continuation() {
            continue;
        }
        let mut foreground = Color::from(cell.fgcolor());
        let mut background = Color::from(cell.bgcolor());
        // A swap carries inverse whenever either side names a colour. When
        // neither does, it carries nothing, and the flag is what survives.
        let unresolvable =
            cell.inverse() && foreground == Color::Default && background == Color::Default;
        if cell.inverse() {
            std::mem::swap(&mut foreground, &mut background);
        }
        // A cell that asked for no particular colour carries none, so an
        // untouched screen is styleless and trailing blanks can be dropped.
        let style = Style {
            foreground: foreground.specified(),
            background: background.specified(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: unresolvable,
        };
        let text = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        match runs.last_mut() {
            Some(run) if run.style == style => run.text.push_str(text),
            _ => runs.push(Run {
                text: text.to_owned(),
                style,
            }),
        }
    }
    // Trailing blanks are the majority of most screens and carry no meaning a
    // viewer needs; a row of them becomes no runs at all.
    while runs
        .last()
        .is_some_and(|run| is_plain(&run.style) && run.text.trim_end().is_empty())
    {
        runs.pop();
    }
    if let Some(run) = runs.last_mut()
        && is_plain(&run.style)
    {
        let trimmed = run.text.trim_end().to_owned();
        run.text = trimmed;
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::{Color, Diff, Snapshot, Style};

    fn screen(width: u16, height: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(height, width, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn plain_text_becomes_one_run_and_blank_rows_vanish() {
        let parser = screen(20, 3, b"hello");
        let snapshot = Snapshot::of(parser.screen(), 0);
        assert_eq!(snapshot.width, 20);
        assert_eq!(snapshot.height, 3);
        assert_eq!(snapshot.rows[0].len(), 1);
        assert_eq!(snapshot.rows[0][0].text, "hello");
        // Trailing blanks carry nothing a viewer needs, and they are most of a
        // terminal.
        assert!(snapshot.rows[1].is_empty());
        assert!(snapshot.rows[2].is_empty());
    }

    #[test]
    fn a_style_change_splits_a_row_into_runs() {
        // "no" plain, "YES" bold red, "no" plain again.
        let parser = screen(20, 1, b"no\x1b[1;31mYES\x1b[0mno");
        let snapshot = Snapshot::of(parser.screen(), 0);
        let row = &snapshot.rows[0];
        assert_eq!(row.len(), 3, "{row:?}");
        assert_eq!(row[0].text, "no");
        assert_eq!(row[1].text, "YES");
        assert!(row[1].style.bold);
        assert_eq!(row[1].style.foreground, Some(Color::Indexed { index: 1 }));
        assert_eq!(row[2].text, "no");
        assert!(!row[2].style.bold);
    }

    #[test]
    fn inverse_video_is_resolved_rather_than_left_for_the_viewer() {
        // A viewer that does not interpret escapes cannot apply inverse itself,
        // so the swap happens here, exactly as the terminal UI does it.
        let parser = screen(10, 1, b"\x1b[31;44;7mx");
        let snapshot = Snapshot::of(parser.screen(), 0);
        let style = snapshot.rows[0][0].style;
        assert_eq!(style.foreground, Some(Color::Indexed { index: 4 }));
        assert_eq!(style.background, Some(Color::Indexed { index: 1 }));
    }

    #[test]
    fn inverse_over_default_colours_survives_as_itself() {
        // The case a swap cannot express. `\x1b[7m` with no colours set is how
        // selections, status lines and highlighted rows are usually drawn, and
        // exchanging two default colours yields a default colour — so without
        // this the run comes back plain and a viewer paints ordinary text where
        // the terminal showed a highlight.
        let parser = screen(10, 1, b"\x1b[7minv");
        let snapshot = Snapshot::of(parser.screen(), 0);
        let run = &snapshot.rows[0][0];
        assert_eq!(run.text, "inv");
        assert!(run.style.inverse, "{run:?}");
        assert_eq!(run.style.foreground, None);
        assert_eq!(run.style.background, None);

        // A swap that does say something is still resolved rather than
        // deferred: one named colour is enough to express it.
        let parser = screen(10, 1, b"\x1b[31;7mx");
        let style = Snapshot::of(parser.screen(), 0).rows[0][0].style;
        assert!(!style.inverse, "{style:?}");
        assert_eq!(style.background, Some(Color::Indexed { index: 1 }));

        // And a plain run is still plain, so trailing blanks still vanish.
        let parser = screen(10, 2, b"plain");
        let snapshot = Snapshot::of(parser.screen(), 0);
        assert!(!snapshot.rows[0][0].style.inverse);
        assert!(snapshot.rows[1].is_empty());
    }

    #[test]
    fn a_hidden_cursor_is_absent_rather_than_placed() {
        let visible = screen(10, 2, b"hi");
        assert!(Snapshot::of(visible.screen(), 0).cursor.is_some());
        let hidden = screen(10, 2, b"hi\x1b[?25l");
        assert!(Snapshot::of(hidden.screen(), 0).cursor.is_none());
    }

    #[test]
    fn a_diff_applies_only_to_the_update_it_followed() {
        let first = Snapshot::of(screen(20, 2, b"one\r\ntwo").screen(), 7);
        let second = Snapshot::of(screen(20, 2, b"one\r\nTWO").screen(), 8);
        let diff = Diff::between(&first, &second).unwrap();
        assert_eq!(diff.follows, 7);
        assert_eq!(diff.sequence, 8);
        assert_eq!(diff.rows.len(), 1, "only the changed row travels");
        assert_eq!(diff.rows[0].row, 1);

        let mut held = first.clone();
        held.apply(&diff).unwrap();
        assert_eq!(held, second, "applying the diff reproduces the screen");

        // A client that missed an update refuses rather than painting a grid
        // that stays wrong.
        let stale = Snapshot::of(screen(20, 2, b"one\r\ntwo").screen(), 6);
        let error = stale.clone().apply(&diff).unwrap_err();
        assert_eq!(error.held, 6);
        assert_eq!(error.expected, 7);
    }

    #[test]
    fn a_full_screen_costs_far_less_than_an_emulator_would() {
        // The reason to render here rather than in the page is that a viewer
        // then carries no emulator. That only holds if what it carries instead
        // stays small, so the size is asserted rather than assumed.
        let mut content = Vec::new();
        for row in 0..50 {
            content.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            content.extend_from_slice(b"\x1b[1;32m$\x1b[0m cargo test --workspace ");
            content.extend_from_slice(b"\x1b[33mwarning\x1b[0m: unused variable `count`");
        }
        let parser = screen(180, 50, &content);
        let snapshot = Snapshot::of(parser.screen(), 1);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(
            json.len() < 32 * 1024,
            "a full 180x50 screen serialized to {} bytes",
            json.len()
        );

        // And a typical update touches one row, not the screen.
        let mut later = content.clone();
        later.extend_from_slice(b"\x1b[50;1Hdone");
        let next = Snapshot::of(screen(180, 50, &later).screen(), 2);
        let diff = Diff::between(&snapshot, &next).unwrap();
        let diff_json = serde_json::to_string(&diff).unwrap();
        assert!(
            diff_json.len() < json.len() / 4,
            "a one-row change serialized to {} bytes against a {} byte screen",
            diff_json.len(),
            json.len()
        );
    }

    #[test]
    fn a_resize_has_no_diff_only_a_snapshot() {
        let narrow = Snapshot::of(screen(20, 2, b"hi").screen(), 1);
        let wide = Snapshot::of(screen(40, 2, b"hi").screen(), 2);
        assert!(
            Diff::between(&narrow, &wide).is_none(),
            "every row describes different cells after a resize"
        );
    }

    #[test]
    fn only_rows_that_differ_are_reported() {
        let first = Snapshot::of(screen(20, 3, b"one\r\n\r\nthree").screen(), 0);
        let second = Snapshot::of(screen(20, 3, b"one\r\n\r\nTHREE").screen(), 0);
        assert_eq!(first.changed_rows(&second), vec![2]);
        assert!(first.changed_rows(&first).is_empty());
        // A resize describes different cells, so every row is reported.
        let resized = Snapshot::of(screen(30, 3, b"one\r\n\r\nthree").screen(), 0);
        assert_eq!(resized.changed_rows(&first), vec![0, 1, 2]);
    }

    #[test]
    fn a_wide_character_is_emitted_once() {
        let parser = screen(10, 1, "全".as_bytes());
        let snapshot = Snapshot::of(parser.screen(), 0);
        assert_eq!(snapshot.rows[0][0].text, "全");
    }

    #[test]
    fn a_default_style_costs_nothing_on_the_wire() {
        let snapshot = Snapshot::of(screen(20, 2, b"plain").screen(), 0);
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("foreground"), "{json}");
        assert!(!json.contains("bold"), "{json}");
        assert_eq!(Style::default(), snapshot.rows[0][0].style);
    }
}
