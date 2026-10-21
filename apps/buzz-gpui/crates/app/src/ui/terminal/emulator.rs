//! The terminal's screen state: alacritty_terminal keeps the VT grid,
//! scrollback and selection; this module feeds it PTY output and turns
//! its visible grid into a frame to paint, coloured the way buzz-app's
//! xterm.js theme colours it (`appearance.ts`).

use super::keys::KeyModes;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use buzz_ui::Colors;
use gpui_kit::{Hsla, Rgba};
use std::sync::{Arc, Mutex};

/// xterm.js's `scrollback` in buzz-app's renderer.
const SCROLLBACK: usize = 5000;
/// xterm.js's `minimumContrastRatio`.
const MINIMUM_CONTRAST: f32 = 4.5;

/// xterm.js's default ANSI palette, which buzz-app keeps: theme roles such
/// as the standard text colour could match a CLI's own pairs.
const ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xe5e5e5,
];

/// The terminal's theme roles (`terminalAppearance`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub cursor_accent: Rgb,
    pub selection_background: Rgb,
    pub selection_foreground: Rgb,
}

impl Theme {
    pub fn from_colors(colors: &Colors) -> Self {
        Self {
            background: rgb(colors.surface_panel),
            foreground: rgb(colors.text_standard),
            cursor: rgb(colors.text_standard),
            cursor_accent: rgb(colors.surface_panel),
            selection_background: rgb(colors.affordance_accent),
            selection_foreground: rgb(colors.text_standard),
        }
    }
}

fn rgb(color: Hsla) -> Rgb {
    let color = Rgba::from(color);
    let byte = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    Rgb {
        r: byte(color.r),
        g: byte(color.g),
        b: byte(color.b),
    }
}

pub fn hsla(color: Rgb) -> Hsla {
    Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

/// A 256-colour palette index as xterm draws it.
fn indexed(index: u8) -> Rgb {
    let hex = |value: u32| Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    };
    match index {
        0..=15 => hex(ANSI[index as usize]),
        16..=231 => {
            let step = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            let index = index - 16;
            Rgb {
                r: step(index / 36),
                g: step(index / 6 % 6),
                b: step(index % 6),
            }
        }
        _ => {
            let gray = 8 + (index - 232) * 10;
            Rgb {
                r: gray,
                g: gray,
                b: gray,
            }
        }
    }
}

fn luminance(color: Rgb) -> f32 {
    let channel = |value: u8| {
        let value = value as f32 / 255.;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// xterm.js's `ensureContrastRatio`: moves the foreground away from the
/// background in 10% steps until it reads, darker first on a lighter
/// background.
pub fn ensure_contrast(background: Rgb, foreground: Rgb, ratio: f32) -> Rgb {
    if contrast(background, foreground) >= ratio {
        return foreground;
    }
    let darker = |mut color: Rgb| {
        while contrast(background, color) < ratio && (color.r | color.g | color.b) != 0 {
            let step = |value: u8| value - (value as f32 * 0.1).ceil().min(value as f32) as u8;
            color = Rgb {
                r: step(color.r),
                g: step(color.g),
                b: step(color.b),
            };
        }
        color
    };
    let lighter = |mut color: Rgb| {
        while contrast(background, color) < ratio && (color.r & color.g & color.b) != 255 {
            let step = |value: u8| value + ((255 - value) as f32 * 0.1).ceil() as u8;
            color = Rgb {
                r: step(color.r),
                g: step(color.g),
                b: step(color.b),
            };
        }
        color
    };
    let (first, second): (Rgb, Rgb) = if luminance(foreground) < luminance(background) {
        let dark = darker(foreground);
        if contrast(background, dark) >= ratio {
            return dark;
        }
        (dark, lighter(foreground))
    } else {
        let light = lighter(foreground);
        if contrast(background, light) >= ratio {
            return light;
        }
        (light, darker(foreground))
    };
    if contrast(background, first) >= contrast(background, second) {
        first
    } else {
        second
    }
}

/// Collects what the emulator asks of its owner while parsing.
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

struct Size {
    columns: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// What parsing output asks the app to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// The emulator's replies to the shell (status reports, colour
    /// queries), which belong to the PTY whoever is looking.
    pub replies: Vec<String>,
    /// OSC 52 text for the clipboard, only while the screen has focus:
    /// a session in the background must not fill the clipboard unseen.
    pub clipboard: Option<String>,
}

/// One painted cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: char,
    /// Combining characters drawn with it.
    pub extra: Option<Vec<char>>,
    pub foreground: Rgb,
    /// Only when it differs from the terminal's background.
    pub background: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// Takes two columns; the next cell is its spacer.
    pub wide: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Underline,
    Beam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub column: usize,
    pub line: usize,
    pub style: CursorStyle,
}

/// The visible grid, resolved for painting.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub columns: usize,
    pub lines: Vec<Vec<Cell>>,
    pub cursor: Option<Cursor>,
    pub theme: Theme,
}

pub struct Screen {
    term: Term<Listener>,
    parser: Processor,
    events: Listener,
    /// Some output has been drawn: the splash may show.
    pub received_output: bool,
    /// The splash has shown, or its first chance passed.
    pub splash_consumed: bool,
    /// Counts the changes [`Screen::frame`] could show, so a view redraws
    /// a cached frame until the next one.
    revision: u64,
}

impl Screen {
    pub fn new(columns: usize, lines: usize) -> Self {
        let events = Listener::default();
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let size = Size {
            columns: columns.max(2),
            lines: lines.max(1),
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            received_output: false,
            splash_consumed: false,
            revision: 0,
        }
    }

    /// Parses PTY output. `theme` answers colour queries and `cell` (in
    /// pixels) size queries.
    pub fn feed(&mut self, bytes: &[u8], theme: &Theme, cell: (u16, u16)) -> Effects {
        if !bytes.is_empty() {
            self.received_output = true;
            self.revision += 1;
        }
        self.parser.advance(&mut self.term, bytes);
        let events = std::mem::take(&mut *self.events.0.lock().unwrap_or_else(|p| p.into_inner()));
        let mut effects = Effects::default();
        for event in events {
            match event {
                Event::PtyWrite(text) => effects.replies.push(text),
                Event::ColorRequest(index, format) => {
                    effects.replies.push(format(self.color_at(index, theme)));
                }
                Event::TextAreaSizeRequest(format) => {
                    let (columns, lines) = self.size();
                    effects.replies.push(format(WindowSize {
                        num_lines: lines as u16,
                        num_cols: columns as u16,
                        cell_width: cell.0,
                        cell_height: cell.1,
                    }));
                }
                Event::ClipboardStore(_, text) if self.term.is_focused => {
                    effects.clipboard = Some(text);
                }
                _ => {}
            }
        }
        effects
    }

    fn color_at(&self, index: usize, theme: &Theme) -> Rgb {
        if let Some(color) = self.term.colors()[index] {
            return color;
        }
        match index {
            0..=255 => indexed(index as u8),
            i if i == NamedColor::Foreground as usize => theme.foreground,
            i if i == NamedColor::Background as usize => theme.background,
            i if i == NamedColor::Cursor as usize => theme.cursor,
            _ => theme.foreground,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// True when the size changed.
    pub fn resize(&mut self, columns: usize, lines: usize) -> bool {
        let (columns, lines) = (columns.max(2), lines.max(1));
        if self.size() == (columns, lines) {
            return false;
        }
        self.term.resize(Size { columns, lines });
        self.revision += 1;
        true
    }

    pub fn modes(&self) -> KeyModes {
        KeyModes {
            app_cursor: self.term.mode().contains(TermMode::APP_CURSOR),
        }
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The alternate screen of a full-screen program, where the wheel sends
    /// arrows instead of scrolling history.
    pub fn alternate_scroll(&self) -> bool {
        let mode = self.term.mode();
        mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL)
    }

    /// Whether the program reports mouse clicks, and in SGR form.
    pub fn mouse_reporting(&self) -> Option<bool> {
        let mode = self.term.mode();
        mode.intersects(TermMode::MOUSE_MODE)
            .then(|| mode.contains(TermMode::SGR_MOUSE))
    }

    pub fn focus_reporting(&self) -> bool {
        self.term.mode().contains(TermMode::FOCUS_IN_OUT)
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.term.is_focused = focused;
        self.revision += 1;
    }

    /// Changes whenever the frame may have.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Scrolls history by `lines` (positive is up).
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
        self.revision += 1;
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
        self.revision += 1;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.revision += 1;
    }

    fn point(&self, column: usize, line: usize) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let (columns, lines) = self.size();
        Point::new(
            Line(line.min(lines - 1) as i32 - offset),
            Column(column.min(columns - 1)),
        )
    }

    /// Starts a selection at a visible cell: characters for one click,
    /// words for two and lines for three.
    pub fn select_start(&mut self, column: usize, line: usize, side: Side, clicks: usize) {
        let ty = match clicks {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let point = self.point(column, line);
        self.term.selection = Some(Selection::new(ty, point, side));
        self.revision += 1;
    }

    pub fn select_update(&mut self, column: usize, line: usize, side: Side) {
        let point = self.point(column, line);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
            self.revision += 1;
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
        self.revision += 1;
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    /// The visible grid with colours resolved against `theme`.
    pub fn frame(&self, theme: &Theme) -> Frame {
        let content = self.term.renderable_content();
        let (columns, lines) = self.size();
        let offset = content.display_offset as i32;
        let blank = Cell {
            text: ' ',
            extra: None,
            foreground: theme.foreground,
            background: None,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide: false,
        };
        let mut rows = vec![vec![blank; columns]; lines];
        let selection = content.selection;
        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset;
            let column = indexed.point.column.0;
            if line < 0 || line as usize >= lines || column >= columns {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            if flags.contains(Flags::WIDE_CHAR_SPACER) {
                rows[line as usize][column].text = ' ';
                continue;
            }
            let mut foreground = self.resolve(cell.fg, theme, flags, true);
            let mut background = self.resolve(cell.bg, theme, Flags::empty(), false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut foreground, &mut background);
            }
            let selected = selection.is_some_and(|range| range.contains(indexed.point));
            if selected {
                foreground = theme.selection_foreground;
                background = theme.selection_background;
            } else {
                foreground = ensure_contrast(background, foreground, MINIMUM_CONTRAST);
            }
            if flags.contains(Flags::HIDDEN) {
                foreground = background;
            }
            rows[line as usize][column] = Cell {
                text: if cell.c == '\t' { ' ' } else { cell.c },
                extra: cell.zerowidth().map(<[char]>::to_vec),
                foreground,
                background: (background != theme.background).then_some(background),
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikeout: flags.contains(Flags::STRIKEOUT),
                wide: flags.contains(Flags::WIDE_CHAR),
            };
        }
        let cursor_line = content.cursor.point.line.0 + offset;
        let style = match content.cursor.shape {
            CursorShape::Hidden => None,
            CursorShape::Underline => Some(CursorStyle::Underline),
            CursorShape::Beam => Some(CursorStyle::Beam),
            CursorShape::Block | CursorShape::HollowBlock => Some(CursorStyle::Block),
        };
        let cursor = style
            .filter(|_| cursor_line >= 0 && (cursor_line as usize) < lines)
            .map(|style| Cursor {
                column: content.cursor.point.column.0.min(columns - 1),
                line: cursor_line as usize,
                style,
            });
        Frame {
            columns,
            lines: rows,
            cursor,
            theme: *theme,
        }
    }

    fn resolve(&self, color: Color, theme: &Theme, flags: Flags, foreground: bool) -> Rgb {
        let colors = self.term.colors();
        match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(index) => {
                // Bold brightens the first eight colours, as in xterm.js.
                let index = if foreground && flags.contains(Flags::BOLD) && index < 8 {
                    index + 8
                } else {
                    index
                };
                colors[index as usize].unwrap_or_else(|| indexed(index))
            }
            Color::Named(named) => {
                let index = named as usize;
                if let Some(color) = colors[index] {
                    return color;
                }
                match named {
                    NamedColor::Foreground | NamedColor::BrightForeground => theme.foreground,
                    NamedColor::DimForeground => dim(theme.foreground),
                    NamedColor::Background => theme.background,
                    NamedColor::Cursor => theme.cursor,
                    _ if index < 16 => {
                        let index = if foreground && flags.contains(Flags::BOLD) && index < 8 {
                            index + 8
                        } else {
                            index
                        };
                        indexed(index as u8)
                    }
                    // The dim variants follow the bright ones in the enum.
                    _ => dim(indexed((index - NamedColor::DimBlack as usize).min(7) as u8)),
                }
            }
        }
    }
}

fn dim(color: Rgb) -> Rgb {
    let half = |value: u8| (value as u16 * 2 / 3) as u8;
    Rgb {
        r: half(color.r),
        g: half(color.g),
        b: half(color.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn theme() -> Theme {
        Theme::from_colors(&Colors::light())
    }

    fn text(frame: &Frame, line: usize) -> String {
        frame.lines[line]
            .iter()
            .map(|cell| cell.text)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn only_a_focused_screen_copies_to_the_clipboard() {
        let mut screen = Screen::new(20, 3);
        let theme = theme();
        let store = b"\x1b]52;c;aGk=\x07";
        assert_eq!(screen.feed(store, &theme, (7, 18)).clipboard, None);
        screen.set_focused(true);
        assert_eq!(
            screen.feed(store, &theme, (7, 18)).clipboard.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn output_lands_on_the_grid_and_scrolls_into_history() {
        let mut screen = Screen::new(20, 3);
        let theme = theme();
        assert!(!screen.received_output);
        screen.feed(b"one\r\ntwo\r\nthree\r\nfour", &theme, (7, 18));
        assert!(screen.received_output);
        let frame = screen.frame(&theme);
        assert_eq!(text(&frame, 0), "two");
        assert_eq!(text(&frame, 2), "four");
        assert_eq!(
            frame.cursor,
            Some(Cursor {
                column: 4,
                line: 2,
                style: CursorStyle::Block
            })
        );
        screen.scroll(1);
        let frame = screen.frame(&theme);
        assert_eq!(text(&frame, 0), "one");
        // The cursor scrolls with its line.
        assert_eq!(frame.cursor.map(|cursor| cursor.line), None);
        screen.scroll_to_bottom();
        assert_eq!(text(&screen.frame(&theme), 0), "two");
    }

    #[test]
    fn colours_come_from_the_theme_and_xterm_palette() {
        let mut screen = Screen::new(10, 2);
        let theme = theme();
        screen.feed(b"a\x1b[31mb\x1b[44mc\x1b[0;7md", &theme, (7, 18));
        let frame = screen.frame(&theme);
        let row = &frame.lines[0];
        assert_eq!(row[0].foreground, theme.foreground);
        assert_eq!(row[0].background, None);
        // Red reads against white once darkened to xterm's 4.5:1.
        assert!(contrast(theme.background, row[1].foreground) >= MINIMUM_CONTRAST);
        assert_eq!(row[2].background, Some(indexed(4)));
        // Inverse swaps the theme's roles.
        assert_eq!(row[3].background, Some(theme.foreground));
    }

    #[test]
    fn the_emulator_answers_status_queries() {
        let mut screen = Screen::new(10, 2);
        let effects = screen.feed(b"\x1b[6n", &theme(), (7, 18));
        assert_eq!(effects.replies, vec!["\x1b[1;1R".to_string()]);
    }

    #[test]
    fn selections_copy_text_and_resize_keeps_content() {
        let mut screen = Screen::new(20, 2);
        screen.feed(b"hello world", &theme(), (7, 18));
        screen.select_start(0, 0, Side::Left, 1);
        screen.select_update(4, 0, Side::Right);
        assert_eq!(screen.selection_text().as_deref(), Some("hello"));
        screen.select_start(7, 0, Side::Left, 2);
        assert_eq!(screen.selection_text().as_deref(), Some("world"));
        screen.clear_selection();
        assert!(!screen.has_selection());
        assert!(screen.resize(40, 4));
        assert!(!screen.resize(40, 4));
        assert_eq!(screen.size(), (40, 4));
    }

    #[test]
    fn palette_matches_xterm() {
        assert_eq!(
            indexed(1),
            Rgb {
                r: 0xcd,
                g: 0x31,
                b: 0x31
            }
        );
        assert_eq!(indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(indexed(21), Rgb { r: 0, g: 0, b: 255 });
        assert_eq!(indexed(232), Rgb { r: 8, g: 8, b: 8 });
    }
}
