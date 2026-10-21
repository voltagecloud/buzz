//! The composer emoji picker's GIF tab (`GifPicker.tsx`,
//! `Emoji.module.css:72-149`): KLIPY search through the relay, trending
//! for an empty query, a two-column masonry of previews, and "Powered by
//! KLIPY". Choosing one inserts its Markdown image; nothing is uploaded.
//!
//! Development only: `BUZZ_DEV_GIF_FIXTURE` names a saved search
//! response to show instead of asking the relay, for relays (like the
//! local test relay) that do not offer GIFs.

use super::media;
use crate::community::{AppState, Community};
use buzz_relay_client::gifs::{self, Gif};
use buzz_ui::{ActiveTheme, Button, SearchField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use std::time::Duration;

/// `.gifPicker` is `min(21.75rem, 40vh)` tall.
const HEIGHT: f32 = 348.;
const DEBOUNCE: Duration = Duration::from_millis(500);
/// The device's KLIPY customer id (`buzz:klipy-customer-id:v1`).
const CUSTOMER_ID: &str = "klipy-customer-id";

/// Insert this Markdown image.
pub struct PickedGif(pub String);

enum Results {
    Loading,
    Ready(Vec<Gif>),
    Failed(SharedString),
}

pub struct GifPanel {
    community: Entity<Community>,
    search: Entity<InputState>,
    results: Results,
    /// The picker's width, borders excluded.
    width: f32,
    /// Bumped per search, so a slow answer cannot replace a newer one.
    generation: u64,
    _search: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<PickedGif> for GifPanel {}

/// The saved response `BUZZ_DEV_GIF_FIXTURE` names.
fn fixture() -> Option<serde_json::Value> {
    let path = std::env::var_os("BUZZ_DEV_GIF_FIXTURE")?;
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Whether `community` offers GIF search.
pub async fn available(community: &Entity<Community>, cx: &mut AsyncApp) -> bool {
    if fixture().is_some() {
        return true;
    }
    let session = community.read_with(cx, |community, _| community.session.clone());
    session.gif_search_available().await
}

/// The locale KLIPY ranks for, `navigator.language` style.
fn locale() -> String {
    locale_from(std::env::var("LANG").ok())
}

fn locale_from(lang: Option<String>) -> String {
    lang.and_then(|lang| lang.split('.').next().map(|tag| tag.replace('_', "-")))
        .filter(|tag| !tag.is_empty() && tag != "C" && tag != "POSIX")
        .unwrap_or_else(|| "en-US".into())
}

fn customer_id(cx: &App) -> String {
    let store = &cx.global::<AppState>().store;
    if let Ok(Some(id)) = store.device_pref(CUSTOMER_ID) {
        return id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    match store.set_device_pref(CUSTOMER_ID, &id) {
        Ok(()) => id,
        Err(_) => "buzz-desktop".into(),
    }
}

impl GifPanel {
    pub fn new(community: Entity<Community>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search GIFs"));
        let subscription = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.schedule(DEBOUNCE, cx);
            }
        });
        Self {
            community,
            search,
            results: Results::Loading,
            width: 360.,
            generation: 0,
            _search: None,
            _subscription: subscription,
        }
    }

    /// Opens on `query` (the emoji tab's), searching at once.
    pub fn open(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |state, cx| {
            state.set_value(query.to_string(), window, cx);
            state.focus(window, cx);
        });
        self.schedule(Duration::ZERO, cx);
    }

    /// The query, which goes back to the emoji tab.
    pub fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    /// Follows the picker's width.
    pub fn set_width(&mut self, width: f32) {
        self.width = width;
    }

    fn schedule(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if !matches!(self.results, Results::Ready(_)) {
            self.results = Results::Loading;
        }
        let query = self.search.read(cx).value().to_string();
        let session = self.community.read(cx).session.clone();
        let customer = customer_id(cx);
        self._search = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let _ = this.update(cx, |this, cx| {
                this.results = Results::Loading;
                cx.notify();
            });
            let result = match fixture() {
                Some(response) => gifs::parse(&response).map(|found| {
                    let wanted = query.trim().to_lowercase();
                    found
                        .into_iter()
                        .filter(|gif| gif.title.to_lowercase().contains(&wanted))
                        .collect()
                }),
                None => session.search_gifs(&query, &customer, &locale()).await,
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.results = match result {
                        Ok(found) => Results::Ready(found),
                        Err(error) => Results::Failed(error.into()),
                    };
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn grid(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let column = (width - 6.) / 2.;
        match &self.results {
            Results::Loading => {
                // Eight placeholder tiles; the 1st and 4th of every four
                // are taller.
                let tile = |index: usize| {
                    let tall = matches!(index % 4, 0 | 3);
                    div()
                        .w_full()
                        .h(px(if tall { 112. } else { 82. }))
                        .rounded(px(8.))
                        .bg(c.affordance_subtle_hover)
                };
                div()
                    .flex()
                    .gap(px(6.))
                    .children((0..2).map(|side| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .w(px(column))
                            .children((0..4).map(move |row| tile(row * 2 + side)))
                    }))
                    .into_any_element()
            }
            Results::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(12.))
                .min_h(px(180.))
                .text_center()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(error.clone())
                .child(
                    Button::new("gif-retry", "Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.schedule(Duration::ZERO, cx))),
                )
                .into_any_element(),
            Results::Ready(found) if found.is_empty() => div()
                .flex()
                .items_center()
                .justify_center()
                .min_h(px(180.))
                .text_body_sm()
                .text_color(c.text_subtle)
                .child("No GIFs found.")
                .into_any_element(),
            Results::Ready(found) => {
                // CSS columns: fill the shorter column next.
                let media = self.community.read(cx).session.media().clone();
                let mut columns: [Vec<AnyElement>; 2] = [Vec::new(), Vec::new()];
                let mut heights = [0f32; 2];
                for (index, gif) in found.iter().enumerate() {
                    let height = column * (gif.preview.height / gif.preview.width) as f32;
                    let side = usize::from(heights[1] < heights[0]);
                    heights[side] += height + 6.;
                    let markdown = gifs::markdown(gif);
                    columns[side].push(
                        div()
                            .id(("gif", index))
                            .relative()
                            .w_full()
                            .h(px(height))
                            .rounded(px(8.))
                            .overflow_hidden()
                            .bg(c.affordance_subtle_hover)
                            .cursor_pointer()
                            .tooltip(buzz_ui::Tooltip::text(gif.title.clone()))
                            .child(
                                img(media::image(&gif.preview.url, &media))
                                    .size_full()
                                    .rounded(px(8.))
                                    .object_fit(ObjectFit::Cover),
                            )
                            // `filter: brightness(1.08)` on hover.
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .rounded(px(8.))
                                    .hover(|s| s.bg(hsla(0., 0., 1., 0.08))),
                            )
                            .on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.emit(PickedGif(markdown.clone()))
                                }),
                            )
                            .into_any_element(),
                    );
                }
                let [left, right] = columns;
                div()
                    .flex()
                    .items_start()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .w(px(column))
                            .children(left),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .w(px(column))
                            .children(right),
                    )
                    .into_any_element()
            }
        }
    }

    /// The tab's body at `width`, below the picker's tabs.
    pub fn render_panel(&mut self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let height = HEIGHT.min(f32::from(window.viewport_size().height) * 0.4);
        let grid = self.grid(width - 24., cx);
        div()
            .flex()
            .flex_col()
            .h(px(height))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(12.))
                    .child(SearchField::new(&self.search).capsule()),
            )
            .child(
                div()
                    .id("gif-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(12.))
                    .pb(px(8.))
                    .child(grid),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .h(px(40.))
                    .border_t_1()
                    .border_color(c.border_standard)
                    .text_caption()
                    .text_color(c.text_subtle)
                    .child("Powered by KLIPY"),
            )
    }
}

impl Render for GifPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_panel(self.width, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn reads_the_locale_from_lang() {
        assert_eq!(locale_from(Some("de_DE.UTF-8".into())), "de-DE");
        assert_eq!(locale_from(Some("C".into())), "en-US");
        assert_eq!(locale_from(None), "en-US");
    }
}
