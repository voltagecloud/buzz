//! The video controls (`VideoControls`), shared by the inline preview and
//! the viewer: play (viewer only), a volume button with a hover popover
//! holding a vertical slider and mute, the elapsed time, the timeline with
//! its hover time and comment markers, the length and the speed menu.

use super::seek::{self, OnHover, OnSeek, seek_area_hover, seek_area_vertical};
use super::video::{Popup, SPEEDS, VideoPlayer, playback_speed};
use super::{tabular, video_time};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, Button, ButtonSize, ButtonVariant, Icon, IconButton,
    IconButtonSize, IconName, MaterialStyled, Tooltip, TypeStyled,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::rc::Rc;

impl VideoPlayer {
    /// The control row. `inline` drops the play button (the preview has
    /// its own in the middle) and draws its own media glass.
    pub(super) fn controls(
        &mut self,
        inline: bool,
        narrow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let rate = playback_speed(cx);
        let duration = self.duration();
        let silent = self.muted || self.volume == 0.;
        let row = div()
            .id(if inline {
                "inline-controls"
            } else {
                "controls"
            })
            .flex()
            .items_center()
            .min_w_0()
            .text_color(c.text_standard)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .map(|this| {
                if inline {
                    this.gap(px(4.))
                        .px(px(6.))
                        .when(!narrow, |d| d.py(px(4.)))
                        .rounded(px(10.))
                        .bg(c.bg_media_glass)
                        .border_1()
                        .border_color(c.border_media_glass)
                } else {
                    this.gap(px(8.)).when(narrow, |d| d.flex_wrap())
                }
            });
        let play = (!inline).then(|| {
            IconButton::new(
                "play",
                if self.playing {
                    IconName::PauseFilled
                } else {
                    IconName::PlayFilled
                },
            )
            .size(IconButtonSize::Sm)
            .tooltip(if self.playing {
                "Pause video"
            } else {
                "Play video"
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
        });
        let timeline = self.timeline(inline, duration, narrow && !inline, cx);
        row.children(play)
            .child(self.volume(silent, window, cx))
            .child(
                div()
                    .flex_none()
                    .text_caption()
                    .line_height(px(12.))
                    .font_features(tabular())
                    .child(video_time(self.position)),
            )
            .child(timeline)
            .when(!(inline && narrow), |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_caption()
                        .line_height(px(12.))
                        .font_features(tabular())
                        .text_color(c.text_subtle)
                        .child(video_time(duration.unwrap_or(0.))),
                )
            })
            .child(self.speed(rate, window, cx))
            .into_any_element()
    }

    /// The timeline: track, hover preview, markers and the scrub area. It
    /// takes focus and steps by a tenth of a second with the arrows.
    fn timeline(
        &mut self,
        inline: bool,
        duration: Option<f64>,
        own_row: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let progress = match duration {
            Some(duration) if duration > 0. => (self.position / duration).clamp(0., 1.) as f32,
            _ => 0.,
        };
        let player = cx.entity().downgrade();
        let seek: Option<OnSeek> = duration.map(|duration| {
            let player = player.clone();
            Rc::new(move |fraction: f32, _: &mut Window, cx: &mut App| {
                let _ = player.update(cx, |this, cx| {
                    this.seek_to(fraction as f64 * duration, cx);
                });
            }) as OnSeek
        });
        let hover_player = player.clone();
        let on_hover: OnHover = Rc::new(move |fraction, _, cx| {
            let _ = hover_player.update(cx, |this, cx| {
                if this.hover != fraction {
                    this.hover = fraction;
                    cx.notify();
                }
            });
        });
        let hover = self.hover.zip(duration).filter(|(_, d)| *d > 0.);
        let markers = duration
            .filter(|d| *d > 0.)
            .map(|duration| {
                self.markers
                    .iter()
                    .filter(|marker| marker.seconds <= duration)
                    .map(|marker| {
                        let at = (marker.seconds / duration) as f32;
                        let (id, seconds) = (marker.id, marker.seconds);
                        let on_marker = self.on_marker.clone();
                        let mut avatar =
                            Avatar::new(marker.author.clone()).size(AvatarSize::Px(32.));
                        if let Some(picture) = marker.picture.clone() {
                            avatar = avatar.image(picture);
                        }
                        let title =
                            format!("{} · {}: {}", marker.label, marker.author, marker.text);
                        div()
                            .absolute()
                            .top(relative(0.5))
                            .left(relative(at))
                            .w_0()
                            .h_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .id(SharedString::from(format!("marker-{}", id.to_hex())))
                                    .flex_none()
                                    .size(px(32.))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .tooltip(Tooltip::text(title))
                                    .child(avatar)
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.seek_to(seconds, cx);
                                        if let Some(on_marker) = on_marker.clone() {
                                            on_marker(id, window, cx);
                                        }
                                    })),
                            )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let step_duration = duration.unwrap_or(0.);
        div()
            .id(if inline {
                "inline-timeline"
            } else {
                "timeline"
            })
            .relative()
            .flex_1()
            .min_w(px(20.))
            .h(px(32.))
            .when(own_row, |d| d.w_full().flex_basis(relative(1.)))
            .rounded(px(8.))
            .track_focus(&self.timeline_focus.clone().tab_stop(true))
            .focus_visible(|s| s.border_2().border_color(c.border_focus))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if let Some(next) =
                    seek::key_step(&event.keystroke.key, this.position, step_duration, 0.1)
                    && step_duration > 0.
                {
                    this.seek_to(next, cx);
                    cx.stop_propagation();
                }
            }))
            .child(super::audio::track_line(
                progress,
                c.bg_media_track,
                c.text_standard,
            ))
            .child(seek_area_hover(
                if inline { "inline-seek" } else { "seek" },
                seek,
                on_hover,
            ))
            .children(hover.map(|(fraction, duration)| hover_time(fraction, duration, &c)))
            .children(markers)
            .into_any_element()
    }

    /// The speaker button and its hover popover: a vertical slider over
    /// the mute button.
    fn volume(&mut self, silent: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let open = self.popup == Some(Popup::Volume);
        let audible = if self.muted { 0. } else { self.volume };
        let player = cx.entity().downgrade();
        let set: seek::OnSeek = Rc::new(move |fraction, _, cx| {
            let _ = player.update(cx, |this, cx| {
                this.set_volume((fraction * 20.).round() / 20., cx)
            });
        });
        let speaker = || {
            if silent {
                IconName::SpeakerSlash
            } else {
                IconName::SpeakerHigh
            }
        };
        let popover = open.then(|| {
            div().absolute().top_0().left(relative(0.5)).child(
                deferred(
                    anchored()
                        .anchor(Anchor::BottomCenter)
                        .offset(point(px(0.), px(-8.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .id("volume-popover")
                                .occlude()
                                .flex()
                                .flex_col()
                                .items_center()
                                .p(px(4.))
                                .bg(c.surface_popover)
                                .border_1()
                                .border_color(c.border_standard)
                                .rounded_full()
                                .text_color(c.text_standard)
                                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                    this.hover_popup(Popup::Volume, *hovered, cx)
                                }))
                                .child(
                                    div()
                                        .id("volume-slider")
                                        .relative()
                                        .w(px(32.))
                                        .h(px(96.))
                                        .my(px(4.))
                                        .rounded(px(8.))
                                        .track_focus(&self.volume_focus.clone().tab_stop(true))
                                        .focus_visible(|s| {
                                            s.border_2().border_color(c.border_focus)
                                        })
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, _, cx| {
                                                let audible =
                                                    if this.muted { 0. } else { this.volume };
                                                if let Some(next) = seek::key_step(
                                                    &event.keystroke.key,
                                                    audible as f64,
                                                    1.,
                                                    0.05,
                                                ) {
                                                    this.set_volume(next as f32, cx);
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .child(volume_track(audible, &c))
                                        .child(seek_area_vertical("volume", set)),
                                )
                                .child(
                                    IconButton::new("mute", speaker())
                                        .size(IconButtonSize::Sm)
                                        .tooltip(if silent { "Unmute video" } else { "Mute video" })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.toggle_mute(cx)),
                                        ),
                                ),
                        ),
                )
                .with_priority(3),
            )
        });
        let _ = window;
        div()
            .id("volume")
            .relative()
            .flex_none()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hover_popup(Popup::Volume, *hovered, cx)
            }))
            .child(
                IconButton::new("volume-button", speaker())
                    .size(IconButtonSize::Sm)
                    .tooltip("Video volume")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.popup = match this.popup {
                            Some(Popup::Volume) => None,
                            _ => Some(Popup::Volume),
                        };
                        cx.notify();
                    })),
            )
            .children(popover)
            .into_any_element()
    }

    /// The `1x` button and its hover menu of speeds, above it.
    fn speed(&mut self, rate: f64, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let open = self.popup == Some(Popup::Speed);
        let _ = window;
        let menu = open.then(|| {
            div().absolute().top_0().left(relative(0.5)).child(
                deferred(
                    anchored()
                        .anchor(Anchor::BottomCenter)
                        .offset(point(px(0.), px(-8.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .id("speed-menu")
                                .occlude()
                                .flex()
                                .flex_col()
                                .floating_surface(&c)
                                .rounded(px(8.))
                                .p(px(2.))
                                .text_caption()
                                .font_features(tabular())
                                .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                    this.hover_popup(Popup::Speed, *hovered, cx)
                                }))
                                .children(SPEEDS.iter().map(|speed| {
                                    let speed = *speed;
                                    div()
                                        .id(SharedString::from(format!("speed-{speed}")))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .h(px(28.))
                                        .px(px(8.))
                                        .rounded(px(5.))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(c.affordance_subtle_hover))
                                        .child(div().flex_1().child(format!("{speed}x")))
                                        .child(div().size(px(16.)).when(speed == rate, |d| {
                                            d.child(Icon::new(IconName::Check).size(16.))
                                        }))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_speed(speed, cx)
                                        }))
                                })),
                        ),
                )
                .with_priority(3),
            )
        });
        div()
            .id("speed")
            .relative()
            .flex_none()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.hover_popup(Popup::Speed, *hovered, cx)
            }))
            .child(
                Button::new("speed-button", format!("{rate}x"))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .tooltip(format!("Playback speed: {rate}x"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.popup = match this.popup {
                            Some(Popup::Speed) => None,
                            _ => Some(Popup::Speed),
                        };
                        cx.notify();
                    })),
            )
            .children(menu)
            .into_any_element()
    }
}

/// The vertical volume track: a 2px rail, its filled part from the bottom
/// and an 8px knob at the level.
fn volume_track(level: f32, c: &buzz_ui::Colors) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(4.))
        .bottom(px(4.))
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(0.5))
                .ml(px(-1.))
                .w(px(2.))
                .rounded_full()
                .bg(c.bg_media_track),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left(relative(0.5))
                .ml(px(-1.))
                .w(px(2.))
                .h(relative(level))
                .rounded_full()
                .bg(c.text_standard),
        )
        .child(
            div()
                .absolute()
                .bottom(relative(level))
                .left(relative(0.5))
                .ml(px(-4.))
                .mb(px(-4.))
                .size(px(8.))
                .rounded_full()
                .bg(c.text_standard),
        )
}

/// `.seekPreview`: a half-strength thumb where the mouse is, with its time
/// in a bubble 12px above it.
pub(super) fn hover_time(fraction: f32, duration: f64, c: &buzz_ui::Colors) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(relative(fraction))
        .w_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .child(
            // Centred above the thumb in a lane wider than any time.
            div()
                .absolute()
                .bottom(px(36.))
                .left(px(-60.))
                .w(px(120.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(c.surface_popover)
                        .text_color(c.text_standard)
                        .text_caption()
                        .whitespace_nowrap()
                        .font_features(tabular())
                        .child(video_time(fraction as f64 * duration)),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(2.))
                .h(px(16.))
                .rounded_full()
                .bg(c.text_standard.opacity(0.5)),
        )
}
