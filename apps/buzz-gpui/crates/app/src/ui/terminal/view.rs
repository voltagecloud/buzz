//! The terminal screen on the page (`renderer.ts`): the emulator's grid
//! drawn in JetBrains Mono with the theme's colours, the keyboard and
//! mouse sent to the shell, resizing to fit, and the welcome splash.

use super::emulator::{Cursor, CursorStyle, Frame, Screen, Theme, hsla};
use super::keys;
use super::splash::{self, Splash};
use alacritty_terminal::index::Side;
use buzz_ui::ActiveTheme;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// xterm.js's cursor blink interval.
const BLINK: Duration = Duration::from_millis(600);
/// xterm.js keeps this much room for its scrollbar when fitting columns.
const SCROLLBAR: f32 = 14.;

/// What the screen sends to its session.
pub enum Input {
    /// Typed or pasted by the person.
    User(Vec<u8>),
    /// The emulator's own answer to the shell.
    Reply(Vec<u8>),
    Resize(usize, usize),
    /// One cell's size in pixels, for size queries.
    Cell(u16, u16),
}

pub type Sink = Rc<dyn Fn(Input, &mut App)>;

/// The measured grid: where it is and how big a cell is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Layout {
    bounds: Bounds<Pixels>,
    cell: Size<Pixels>,
    font_size: Pixels,
}

pub struct TerminalView {
    screen: Entity<Screen>,
    sink: Sink,
    focus: FocusHandle,
    layout: Rc<Cell<Layout>>,
    /// The cursor's blink phase while focused.
    blink_on: bool,
    has_focus: bool,
    /// A drag selecting text, from the cell it started in.
    selecting: bool,
    /// The last frame built, with the screen revision and theme it shows,
    /// so a cursor blink repaints without rebuilding every cell.
    frame: Option<(u64, Theme, Rc<Frame>)>,
    /// Scrolled lines not acted on yet, so slow trackpad scrolling adds
    /// up instead of rounding to nothing on every event.
    scroll_rest: f32,
    splash: Option<Splash>,
    _blink: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(
        screen: Entity<Screen>,
        sink: Sink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let blink = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                // Only a focused cursor blinks, so an idle screen does
                // not redraw.
                if this
                    .update(cx, |this, cx| {
                        if this.has_focus {
                            this.blink_on = !this.blink_on;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let focus_in = cx.on_focus(&focus, window, |this, _, cx| this.focused(true, cx));
        let focus_out = cx.on_blur(&focus, window, |this, _, cx| this.focused(false, cx));
        let output = cx.observe(&screen, |_, _, cx| cx.notify());
        // Leaving the page uses up a splash that has not shown yet, and
        // the screen no longer counts as focused (no blur comes).
        let release = cx.on_release(|this, cx| {
            this.screen.update(cx, |screen, _| {
                screen.splash_consumed = true;
                screen.set_focused(false);
            });
        });
        Self {
            screen,
            sink,
            focus,
            layout: Rc::default(),
            blink_on: true,
            has_focus: false,
            selecting: false,
            frame: None,
            scroll_rest: 0.,
            splash: None,
            _blink: blink,
            _subscriptions: vec![focus_in, focus_out, output, release],
        }
    }

    fn focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        self.blink_on = true;
        self.has_focus = focused;
        let report = self.screen.update(cx, |screen, _| {
            screen.set_focused(focused);
            screen.focus_reporting()
        });
        if report {
            let bytes = if focused { b"\x1b[I" } else { b"\x1b[O" };
            (self.sink)(Input::Reply(bytes.to_vec()), cx);
        }
        cx.notify();
    }

    fn dismiss_splash(&mut self, cx: &mut Context<Self>) {
        if self.splash.take().is_some() {
            cx.notify();
        }
    }

    fn send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.dismiss_splash(cx);
        self.blink_on = true;
        self.screen.update(cx, |screen, _| {
            screen.clear_selection();
            screen.scroll_to_bottom();
        });
        (self.sink)(Input::User(bytes), cx);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let key = keystroke.key.as_str();
        self.dismiss_splash(cx);
        // Copy and paste: Ctrl+Shift+C/V here, Command+C/V on macOS.
        let clipboard_chord = if cfg!(target_os = "macos") {
            modifiers.platform && !modifiers.control && !modifiers.alt
        } else {
            modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.platform
        };
        if clipboard_chord && key == "c" {
            if let Some(text) = self.screen.read(cx).selection_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.stop_propagation();
            return;
        }
        if clipboard_chord && key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let bracketed = self.screen.read(cx).bracketed_paste();
                self.send(keys::paste(&text, bracketed), cx);
            }
            cx.stop_propagation();
            return;
        }
        if modifiers.shift && !modifiers.control && (key == "pageup" || key == "pagedown") {
            self.screen
                .update(cx, |screen, _| screen.scroll_page(key == "pageup"));
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let modes = self.screen.read(cx).modes();
        let encoded = keys::encode(
            key,
            keystroke.key_char.as_deref(),
            modifiers,
            modes,
            cfg!(target_os = "macos"),
        );
        if let Some(bytes) = encoded {
            self.send(bytes, cx);
            cx.stop_propagation();
        } else if key == "escape" {
            // Escape belongs to the shell while it has focus.
            cx.stop_propagation();
        }
    }

    /// The cell under a window position, and which half of it.
    fn cell_at(&self, position: Point<Pixels>, cx: &App) -> (usize, usize, Side) {
        let layout = self.layout.get();
        let (columns, lines) = self.screen.read(cx).size();
        let x = f32::from(position.x - layout.bounds.origin.x).max(0.);
        let y = f32::from(position.y - layout.bounds.origin.y).max(0.);
        let width = f32::from(layout.cell.width).max(1.);
        let height = f32::from(layout.cell.height).max(1.);
        let column = ((x / width) as usize).min(columns.saturating_sub(1));
        let line = ((y / height) as usize).min(lines.saturating_sub(1));
        let side = if x % width < width / 2. {
            Side::Left
        } else {
            Side::Right
        };
        (column, line, side)
    }

    /// A mouse report for programs that asked for one.
    fn report(
        &self,
        button: u8,
        press: bool,
        position: Point<Pixels>,
        cx: &App,
    ) -> Option<Vec<u8>> {
        let sgr = self.screen.read(cx).mouse_reporting()?;
        let (column, line, _) = self.cell_at(position, cx);
        if sgr {
            let end = if press { 'M' } else { 'm' };
            return Some(format!("\x1b[<{button};{};{}{end}", column + 1, line + 1).into_bytes());
        }
        let button = if press { button } else { 3 };
        let byte = |value: usize| (32 + value).min(255) as u8;
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + button,
            byte(column + 1),
            byte(line + 1),
        ])
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if event.button != MouseButton::Left {
            return;
        }
        if !event.modifiers.shift
            && let Some(report) = self.report(0, true, event.position, cx)
        {
            (self.sink)(Input::User(report), cx);
            return;
        }
        let (column, line, side) = self.cell_at(event.position, cx);
        self.selecting = true;
        self.screen.update(cx, |screen, _| {
            screen.select_start(column, line, side, event.click_count)
        });
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let (column, line, side) = self.cell_at(event.position, cx);
        self.screen
            .update(cx, |screen, _| screen.select_update(column, line, side));
        cx.notify();
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        if !self.selecting {
            if let Some(report) = self.report(0, false, event.position, cx) {
                (self.sink)(Input::User(report), cx);
            }
            return;
        }
        self.selecting = false;
        let empty = !self.screen.read(cx).has_selection();
        if empty {
            self.screen.update(cx, |screen, _| screen.clear_selection());
        }
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let height = f32::from(self.layout.get().cell.height).max(1.);
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / height,
        };
        let steps = whole_lines(&mut self.scroll_rest, lines);
        if steps == 0 {
            return;
        }
        let screen = self.screen.read(cx);
        if screen.mouse_reporting().is_some() {
            let button = if steps > 0 { 64 } else { 65 };
            for _ in 0..steps.unsigned_abs() {
                if let Some(report) = self.report(button, true, event.position, cx) {
                    (self.sink)(Input::User(report), cx);
                }
            }
        } else if screen.alternate_scroll() {
            let arrow: &[u8] = match (steps > 0, screen.modes().app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes = arrow.repeat(steps.unsigned_abs() as usize);
            (self.sink)(Input::User(bytes), cx);
        } else {
            self.screen.update(cx, |screen, _| screen.scroll(steps));
        }
        cx.notify();
    }
}

/// Adds `lines` to the fraction left over from earlier events and takes
/// out the whole lines. Turning back drops what was left.
fn whole_lines(rest: &mut f32, lines: f32) -> i32 {
    if *rest * lines < 0. {
        *rest = 0.;
    }
    let total = *rest + lines;
    let steps = total.trunc();
    *rest = total - steps;
    steps as i32
}

/// Fits the grid to the space it has (`FitAddon`).
fn fit(screen: &Entity<Screen>, sink: &Sink, layout: Layout, cx: &mut App) {
    let width = f32::from(layout.bounds.size.width) - SCROLLBAR;
    let height = f32::from(layout.bounds.size.height);
    if width <= 0. || height <= 0. {
        return;
    }
    let columns = (width / f32::from(layout.cell.width)).floor().max(2.) as usize;
    let lines = (height / f32::from(layout.cell.height)).floor().max(1.) as usize;
    if screen.read(cx).size() == (columns, lines) {
        return;
    }
    let screen = screen.clone();
    let sink = sink.clone();
    let cell = (
        f32::from(layout.cell.width).round() as u16,
        f32::from(layout.cell.height).round() as u16,
    );
    cx.defer(move |cx| {
        screen.update(cx, |screen, cx| {
            if screen.resize(columns, lines) {
                cx.notify();
            }
        });
        sink(Input::Cell(cell.0, cell.1), cx);
        sink(Input::Resize(columns, lines), cx);
    });
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The monospace font in one of the terminal's styles.
fn mono(bold: bool, italic: bool) -> Font {
    Font {
        family: buzz_ui::tokens::font::MONO.into(),
        features: FontFeatures::default(),
        fallbacks: Some(buzz_ui::fonts::fallbacks()),
        weight: if bold {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = *cx.colors();
        let theme = Theme::from_colors(&colors);
        let screen = self.screen.read(cx);
        let revision = screen.revision();
        let frame = match &self.frame {
            Some((cached, cached_theme, frame))
                if *cached == revision && *cached_theme == theme =>
            {
                frame.clone()
            }
            _ => {
                let frame = Rc::new(screen.frame(&theme));
                self.frame = Some((revision, theme, frame.clone()));
                frame
            }
        };
        // `text-mono`: 12px JetBrains Mono, in rows of 1.5 lines.
        let font_size = rems(0.75).to_pixels(window.rem_size());
        let font_id = window.text_system().resolve_font(&mono(false, false));
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        let cell = size(cell_width, (font_size * 1.5).round());

        // The splash shows once, after the first output, while on screen.
        let layout = self.layout.get();
        let has_room = layout.bounds.size.width > px(0.) && layout.bounds.size.height > px(0.);
        let ready = {
            let screen = self.screen.read(cx);
            screen.received_output && !screen.splash_consumed
        };
        if self.splash.is_none() && ready && has_room {
            self.screen
                .update(cx, |screen, _| screen.splash_consumed = true);
            self.splash = Some(Splash::new());
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(splash::DURATION).await;
                let _ = this.update(cx, |this, cx| this.dismiss_splash(cx));
            })
            .detach();
        }
        let reduce_motion = cx.reduce_motion();
        if let Some(splash) = &mut self.splash {
            if reduce_motion {
                splash.rest();
            } else {
                splash.tick(Instant::now());
                window.request_animation_frame();
            }
        }

        let focused = self.focus.is_focused(window);
        let blink_on = self.blink_on;
        let layout_cell = self.layout.clone();
        let (screen, sink) = (self.screen.clone(), self.sink.clone());
        let grid = canvas(
            move |bounds, _, cx| {
                let layout = Layout {
                    bounds,
                    cell,
                    font_size,
                };
                layout_cell.set(layout);
                fit(&screen, &sink, layout, cx);
            },
            move |bounds, _, window, cx| {
                paint_frame(
                    &frame,
                    bounds.origin,
                    cell,
                    font_size,
                    focused,
                    blink_on,
                    window,
                    cx,
                );
            },
        )
        .size_full();

        let overlay = self.splash.as_ref().map(|state| {
            let palette = splash::Palette::new(&colors, cx.theme().is_dark());
            let elapsed = state.elapsed;
            let pointer = state.pointer;
            let pulses = state.pulses.clone();
            div()
                .id("terminal-splash")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        // Artwork clicks keep the terminal focused and never
                        // reach the shell as mouse input.
                        window.focus(&this.focus, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                    if cx.reduce_motion() {
                        return;
                    }
                    let bounds = this.layout.get().bounds;
                    let position = event.position();
                    let (x, y) = fraction(bounds, position);
                    if let Some(splash) = &mut this.splash {
                        splash.click(x, y);
                    }
                }))
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    if cx.reduce_motion() {
                        return;
                    }
                    let (x, y) = fraction(this.layout.get().bounds, event.position);
                    if let Some(splash) = &mut this.splash {
                        splash.hover(x, y);
                    }
                }))
                .on_hover(cx.listener(|this, hovered: &bool, _, _| {
                    if !hovered && let Some(splash) = &mut this.splash {
                        splash.leave();
                    }
                }))
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let Some(banner) = splash::build(
                                f32::from(bounds.size.width),
                                f32::from(bounds.size.height),
                            ) else {
                                window.paint_quad(fill(bounds, palette.surface));
                                return;
                            };
                            let mut frame = Splash::new();
                            frame.elapsed = elapsed;
                            frame.pointer = pointer;
                            frame.pulses = pulses;
                            splash::paint(bounds, &banner, &palette, &frame, window);
                        },
                    )
                    .size_full(),
                )
        });

        div()
            .id("terminal-screen")
            .key_context("BuzzTerm")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.surface_panel)
            .text_color(colors.text_standard)
            .cursor(gpui_kit::CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(grid)
            .children(overlay)
    }
}

fn fraction(bounds: Bounds<Pixels>, position: Point<Pixels>) -> (f32, f32) {
    let width = f32::from(bounds.size.width).max(1.);
    let height = f32::from(bounds.size.height).max(1.);
    (
        f32::from(position.x - bounds.origin.x) / width,
        f32::from(position.y - bounds.origin.y) / height,
    )
}

/// Characters drawn as part of a run: one column wide in the mono font.
fn simple(character: char) -> bool {
    (' '..='~').contains(&character)
}

#[allow(clippy::too_many_arguments)]
fn paint_frame(
    frame: &Frame,
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    font_size: Pixels,
    focused: bool,
    blink_on: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let theme = frame.theme;
    let x_of = |column: usize| origin.x + cell.width * column as f32;
    for (row, cells) in frame.lines.iter().enumerate() {
        let y = origin.y + cell.height * row as f32;
        // Backgrounds, merged across equal neighbours.
        let mut column = 0;
        while column < cells.len() {
            let Some(background) = cells[column].background else {
                column += 1;
                continue;
            };
            let start = column;
            while column < cells.len() && cells[column].background == Some(background) {
                column += 1;
            }
            window.paint_quad(fill(
                Bounds::new(
                    point(x_of(start), y),
                    size(cell.width * (column - start) as f32, cell.height),
                ),
                hsla(background),
            ));
        }
        // Text: runs of plain characters in one style, and anything wider
        // or combined on its own, each at its column.
        let mut column = 0;
        while column < cells.len() {
            let first = &cells[column];
            if first.text == ' ' && !first.underline && !first.strikeout {
                column += 1;
                continue;
            }
            let start = column;
            let mut text = String::new();
            if simple(first.text) && first.extra.is_none() {
                while column < cells.len() {
                    let next = &cells[column];
                    let same = next.foreground == first.foreground
                        && next.bold == first.bold
                        && next.italic == first.italic
                        && next.underline == first.underline
                        && next.strikeout == first.strikeout;
                    if !same || !simple(next.text) || next.extra.is_some() {
                        break;
                    }
                    text.push(next.text);
                    column += 1;
                }
            } else {
                text.push(first.text);
                text.extend(first.extra.iter().flatten());
                column += if first.wide { 2 } else { 1 };
            }
            paint_text(
                &text,
                first,
                point(x_of(start), y),
                cell.height,
                font_size,
                window,
                cx,
            );
        }
    }
    let Some(Cursor {
        column,
        line,
        style,
    }) = frame.cursor
    else {
        return;
    };
    let at = point(x_of(column), origin.y + cell.height * line as f32);
    let color = hsla(theme.cursor);
    match style {
        CursorStyle::Block if focused => {
            if !blink_on {
                return;
            }
            window.paint_quad(fill(Bounds::new(at, cell), color));
            let under = &frame.lines[line][column];
            if under.text != ' ' {
                let mut accent = under.clone();
                accent.foreground = theme.cursor_accent;
                let text: String = std::iter::once(under.text)
                    .chain(under.extra.iter().flatten().copied())
                    .collect();
                paint_text(&text, &accent, at, cell.height, font_size, window, cx);
            }
        }
        CursorStyle::Block => {
            window.paint_quad(outline(Bounds::new(at, cell), color, BorderStyle::Solid));
        }
        _ if focused && !blink_on => {}
        CursorStyle::Underline => window.paint_quad(fill(
            Bounds::new(
                point(at.x, at.y + cell.height - px(2.)),
                size(cell.width, px(2.)),
            ),
            color,
        )),
        CursorStyle::Beam => {
            window.paint_quad(fill(Bounds::new(at, size(px(1.), cell.height)), color))
        }
    }
}

fn paint_text(
    text: &str,
    style: &super::emulator::Cell,
    origin: Point<Pixels>,
    line_height: Pixels,
    font_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let color = hsla(style.foreground);
    let run = TextRun {
        len: text.len(),
        font: mono(style.bold, style.italic),
        color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: style.strikeout.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    };
    let shaped = window
        .text_system()
        .shape_line(text.to_string().into(), font_size, &[run], None);
    let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn small_scrolls_add_up_to_lines() {
        let mut rest = 0.;
        assert_eq!(whole_lines(&mut rest, 0.4), 0);
        assert_eq!(whole_lines(&mut rest, 0.4), 0);
        assert_eq!(whole_lines(&mut rest, 0.4), 1);
        assert_eq!(whole_lines(&mut rest, 3.), 3);
        // Turning back starts over.
        assert_eq!(whole_lines(&mut rest, -0.9), 0);
        assert_eq!(whole_lines(&mut rest, -0.2), -1);
    }
}
