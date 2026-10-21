//! The mention picker behind the composer's @ button (`MentionPicker.tsx`):
//! a 380px popover above the composer with "Search community people and
//! agents", the ranked choices with their matched letters underlined, and
//! the directory search's status.

use super::mention_rules;
use super::{Composer, Suggestion, media};
use buzz_relay_client::ChannelType;
use buzz_ui::{ActiveTheme, Avatar, AvatarSize, Button, SearchField, Surface, TypeStyled};
use gpui_kit::component::input::{InlineToken, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

pub struct Picker {
    pub search: Entity<InputState>,
    pub selected: usize,
    _subscription: Subscription,
}

impl Composer {
    /// The @ button: opens the picker, or closes it.
    pub(super) fn toggle_mention_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mention_picker.take().is_some() {
            self.focus(window, cx);
            cx.notify();
            return;
        }
        // The press outside that just closed it.
        if self
            .mention_picker_closed
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
        if self.input_disabled(cx) {
            return;
        }
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search community people and agents")
        });
        search.update(cx, |state, cx| state.focus(window, cx));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let query = this.picker_query(cx);
                    this.search_people(&query, cx);
                    if let Some(picker) = &mut this.mention_picker {
                        picker.selected = 0;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.choose_from_picker(None, window, cx),
                _ => {}
            },
        );
        self.mention_picker = Some(Picker {
            search,
            selected: 0,
            _subscription: subscription,
        });
        self.search_people("", cx);
        self.watch_teams(cx);
        cx.notify();
    }

    /// The picker's saved teams for `query`, and the catalog's status line.
    fn picker_teams(&self, query: &str, cx: &App) -> (Vec<Suggestion>, Option<&'static str>) {
        let (teams, catalog) = self.saved_teams(cx);
        (
            self.team_suggestions(&teams, catalog, query, cx),
            super::teams_status(catalog),
        )
    }

    /// The teams the keyboard can choose: the enabled ones.
    fn picker_team_choices(&self, query: &str, cx: &App) -> Vec<Suggestion> {
        self.picker_teams(query, cx)
            .0
            .into_iter()
            .filter(|team| {
                matches!(
                    team,
                    Suggestion::Team {
                        disabled: false,
                        ..
                    }
                )
            })
            .collect()
    }

    fn picker_query(&self, cx: &App) -> String {
        self.mention_picker
            .as_ref()
            .map(|picker| picker.search.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Inserts the highlighted (or `index`) choice as a mention chip at the
    /// caret (`insertMention`).
    fn choose_from_picker(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.picker_query(cx);
        let found = self.mention_choices(&query, &[], cx);
        let Some(picker) = &self.mention_picker else {
            return;
        };
        let index = index.unwrap_or(picker.selected);
        let Some(person) = found.people.get(index).cloned() else {
            // After the people come the enabled saved teams.
            let teams = self.picker_team_choices(&query, cx);
            if self.editing.is_some() {
                return;
            }
            if let Some(Suggestion::Team { recipients, .. }) =
                teams.into_iter().nth(index - found.people.len())
                && self.insert_team(&recipients, None, window, cx)
            {
                self.mention_picker = None;
            }
            cx.notify();
            return;
        };
        if self.editing.is_none() {
            let recipients = self.recipients(cx);
            if recipients.len() >= 32 && !recipients.iter().any(|(key, _)| *key == person.pubkey) {
                self.error = Some("Choose at most 32 recipients".into());
                cx.notify();
                return;
            }
        }
        self.mention_picker = None;
        let token = if self.editing.is_some() {
            // An edit names people; it never adds recipients.
            crate::ui::message_edit::mention_text(&person.pubkey, &person.name).map(|link| {
                InlineToken::new(person.pubkey.clone(), link)
                    .with_label(format!("@{}", person.name))
            })
        } else {
            Some(InlineToken::new(
                person.pubkey.clone(),
                format!("@{}", person.name),
            ))
        };
        self.input.update(cx, |state, cx| {
            state.focus(window, cx);
            let caret = state.selected_range();
            let before = state.value()[..caret.start].chars().next_back();
            if before.is_some_and(|c| !c.is_whitespace() && !"([{".contains(c)) {
                state.replace(" ", window, cx);
            }
            if let Some(token) = token {
                let at = state.selected_range();
                let _ = state.replace_range_with_token(at, token, window, cx);
                state.insert(" ", window, cx);
            }
        });
        let community = self.community.read(cx);
        let (origin, channel) = (community.origin.http(), self.channel.clone());
        cx.default_global::<super::MentionHistory>()
            .0
            .entry(origin)
            .or_default()
            .remember(&channel, &person.pubkey);
        cx.notify();
    }

    fn move_picker(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.picker_query(cx);
        let count = self.mention_choices(&query, &[], cx).people.len()
            + self.picker_team_choices(&query, cx).len();
        if let Some(picker) = &mut self.mention_picker
            && count > 0
        {
            picker.selected =
                (picker.selected as isize + delta).rem_euclid(count as isize) as usize;
            cx.notify();
        }
    }

    pub(super) fn mention_picker_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.mention_picker.as_ref()?;
        let c = *cx.colors();
        let query = picker.search.read(cx).value().to_string();
        let found = self.mention_choices(&query, &[], cx);
        let community = self.community.read(cx);
        let channel = community
            .snapshot
            .channels
            .iter()
            .find(|c| c.id == self.channel);
        let members = channel.map(|c| c.members.clone()).unwrap_or_default();
        let dm = channel.is_some_and(|c| c.channel_type == Some(ChannelType::Dm));
        let media = community.session.media().clone();
        let searching = found.status == Some("Searching community…");
        let failed = found.retry && found.status != Some("Channel membership unavailable.");
        let caption =
            |text: &'static str| div().text_caption().text_color(c.text_subtle).child(text);
        let rows = found.people.iter().enumerate().map(|(index, person)| {
            let positions = mention_rules::match_person(&person.label, &query)
                .map(|(_, positions)| positions)
                .unwrap_or_default();
            let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = person
                .label
                .char_indices()
                .enumerate()
                .filter(|(n, _)| positions.contains(n))
                .map(|(_, (at, ch))| {
                    (
                        at..at + ch.len_utf8(),
                        HighlightStyle {
                            underline: Some(UnderlineStyle {
                                thickness: px(1.),
                                color: None,
                                wavy: false,
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let mut avatar = Avatar::new(person.label.clone()).size(AvatarSize::Large);
            if person.agent || !person.member {
                avatar = avatar.squircle();
            }
            if let Some(picture) = &person.picture {
                avatar = avatar.image(media::avatar(picture, &media));
            }
            let outside = !members.contains(&person.pubkey);
            let label = div()
                .flex()
                .flex_col()
                .child(StyledText::new(person.label.clone()).with_highlights(highlights))
                .when(outside, |d| {
                    d.child(div().text_caption().text_color(c.text_subtle).child(if dm {
                        "Not in DM · Will not be notified"
                    } else {
                        "Not in channel · Choose whether to add when you send"
                    }))
                });
            // A listbox option row (`NavigationItem variant="option"`).
            div()
                .id(SharedString::from(format!("mention-choice-{index}")))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.))
                .min_h(px(32.))
                .p(px(8.))
                .rounded(px(8.))
                .cursor_pointer()
                .when(index == picker.selected, |d| {
                    d.bg(c.affordance_floating_selected)
                })
                .when(index != picker.selected, |d| {
                    d.hover(|s| s.bg(c.affordance_floating_hover))
                })
                .child(div().flex_none().child(avatar))
                .child(div().flex_1().min_w_0().child(label))
                .child(
                    div()
                        .flex_none()
                        .text_caption()
                        .font_family(buzz_ui::tokens::font::MONO)
                        .text_color(c.text_metadata)
                        .child(person.pubkey[..12].to_string()),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_from_picker(Some(index), window, cx)
                }))
        });
        // Saved teams after the people; only enabled ones take the keyboard.
        let (teams, team_status) = self.picker_teams(&query, cx);
        let people = found.people.len();
        let mut next = people;
        let team_rows: Vec<AnyElement> = teams
            .iter()
            .enumerate()
            .filter_map(|(row, team)| {
                let Suggestion::Team {
                    name,
                    recipients,
                    detail,
                    disabled,
                    ..
                } = team
                else {
                    return None;
                };
                let index = (!*disabled).then(|| {
                    next += 1;
                    next - 1
                });
                let avatars = div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .children(recipients.iter().take(4).map(|(_, name, picture)| {
                        let mut avatar =
                            Avatar::new(name.clone()).size(AvatarSize::Small).squircle();
                        if let Some(picture) = picture {
                            avatar = avatar.image(media::avatar(picture, &media));
                        }
                        avatar
                    }))
                    .when(recipients.len() > 4, |d| {
                        d.child(
                            div()
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child(format!("+{}", recipients.len() - 4)),
                        )
                    });
                let selected = index == Some(picker.selected);
                Some(
                    div()
                        .id(SharedString::from(format!("mention-team-{row}")))
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(8.))
                        .min_h(px(32.))
                        .p(px(8.))
                        .rounded(px(8.))
                        .when(*disabled, |d| d.text_color(c.text_subtle))
                        .when(selected, |d| d.bg(c.affordance_floating_selected))
                        .when_some(index.filter(|_| !selected), |d, _| {
                            d.cursor_pointer()
                                .hover(|s| s.bg(c.affordance_floating_hover))
                        })
                        .child(avatars)
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .child(
                            div()
                                .flex_shrink(1.)
                                .min_w_0()
                                .max_w(px(180.))
                                .text_caption()
                                .text_color(c.text_subtle)
                                .child(detail.clone()),
                        )
                        .when_some(index, |d, index| {
                            d.on_click(cx.listener(move |this, _, window, cx| {
                                this.choose_from_picker(Some(index), window, cx)
                            }))
                        })
                        .into_any_element(),
                )
            })
            .collect();
        let empty =
            !members.is_empty() && found.people.is_empty() && teams.is_empty() && !searching;
        let content = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .p(px(12.))
            .gap(px(8.))
            .child(
                SearchField::new(&picker.search)
                    .capsule()
                    .surface(Surface::Floating),
            )
            .when(searching, |d| d.child(caption("Searching community…")))
            .when(failed, |d| {
                d.child(caption(
                    "Could not search community people. Retry to refresh.",
                ))
                .child(
                    Button::new("retry-mention-search", "Retry community search")
                        .surface(Surface::Floating)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.retry_completion(super::channels::Retry::People, cx);
                            let query = this.picker_query(cx);
                            this.search_people(&query, cx);
                        })),
                )
            })
            .when(
                found.status == Some("Channel membership unavailable."),
                |d| d.child(caption("Channel membership unavailable.")),
            )
            .when_some(team_status, |d, status| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(caption(status))
                        .when(status.starts_with("Could not"), |d| {
                            d.child(
                                Button::new("retry-mention-teams", "Retry teams")
                                    .surface(Surface::Floating)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.retry_completion(super::channels::Retry::Teams, cx)
                                    })),
                            )
                        }),
                )
            })
            .child(
                div()
                    .id("mention-choices")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(4.))
                    .overflow_y_scroll()
                    .children(rows)
                    .children(team_rows)
                    .when(
                        found.status == Some("Narrow your search to see more members."),
                        |d| d.child(caption("Narrow your search to see more members.")),
                    )
                    .when(empty, |d| d.child(caption("No matching channel members."))),
            );
        Some(
            div()
                .absolute()
                .left_0()
                .bottom_full()
                .pb(px(4.))
                .child(
                    div()
                        .id("mention-picker")
                        .occlude()
                        .key_context("MentionPicker")
                        .w(px(380.))
                        .h(px(360.))
                        .overflow_hidden()
                        .rounded(px(16.))
                        .border_1()
                        .border_color(c.border_standard)
                        .bg(c.surface_popover)
                        .shadow(buzz_ui::tokens::shadow::sm())
                        .text_body()
                        .text_color(c.text_standard)
                        .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                            this.move_picker(-1, cx);
                            cx.stop_propagation();
                        }))
                        .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                            this.move_picker(1, cx);
                            cx.stop_propagation();
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                this.mention_picker = None;
                                this.focus(window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.mention_picker = None;
                            this.mention_picker_closed = Some(std::time::Instant::now());
                            cx.notify();
                        }))
                        .child(content),
                )
                .into_any_element(),
        )
    }
}
