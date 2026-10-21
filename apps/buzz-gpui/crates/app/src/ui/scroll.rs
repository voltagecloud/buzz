//! Where a reader left a channel (`ChannelTimeline`'s `scroll:<channel>`
//! view state and its row geometry cache).
//!
//! The position is saved when the page closes and restored when the
//! channel opens again, also after a restart. Within a run the list's
//! measured rows are kept too, so a channel reopened soon paints at once
//! where it was, without the jumps of measuring again.

use super::message::Item;
use crate::community::{AppState, Community};
use buzz_relay_client::EventId;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Channels whose measured rows are kept, as `geometry.ts` keeps three.
const CACHED: usize = 3;

/// A reading position. `anchor` is the row at the top of the view and how
/// far its top sits above the view's top edge (`y` is zero or negative).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Pixels into the list's first row, when that row is the top edge.
    pub offset: f32,
    /// The reader was at the newest message.
    pub bottom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub id: String,
    pub y: f32,
}

fn key(channel: &str) -> String {
    format!("scroll:{channel}")
}

/// The position saved for `channel`, if any.
pub fn load(community: &Entity<Community>, channel: &str, cx: &App) -> Option<Position> {
    let partition = community.read(cx).partition();
    let json = cx
        .global::<AppState>()
        .store
        .view_state(&partition, &key(channel))
        .ok()??;
    serde_json::from_str(&json).ok()
}

pub fn save(community: &Entity<Community>, channel: &str, position: &Position, cx: &App) {
    let partition = community.read(cx).partition();
    let json = serde_json::to_string(position).expect("positions serialize");
    let saved =
        cx.global::<AppState>()
            .store
            .set_view_state(&partition, &key(channel), Some(&json));
    if let Err(error) = saved {
        tracing::warn!(%error, "could not save the reading position");
    }
}

/// The position of a list whose row 0 is the top edge and whose rows
/// `1..` are `items`.
pub fn capture(list: &ListState, items: &[Item]) -> Position {
    let top = list.logical_scroll_top();
    let bottom = list.is_following_tail();
    match top.item_ix.checked_sub(1).and_then(|ix| items.get(ix)) {
        Some(item) => Position {
            offset: 0.,
            bottom,
            anchor: Some(Anchor {
                id: item.row.id.to_hex(),
                y: -f32::from(top.offset_in_item),
            }),
        },
        None => Position {
            offset: f32::from(top.offset_in_item),
            bottom: bottom || top.item_ix > items.len(),
            anchor: None,
        },
    }
}

/// Where `position` puts the list, or `None` to stay at the bottom: the
/// reader was there, or the anchor row is no longer loaded.
pub fn offset(position: &Position, items: &[Item]) -> Option<ListOffset> {
    if position.bottom {
        return None;
    }
    match &position.anchor {
        Some(anchor) => {
            let id = EventId::from_hex(&anchor.id).ok()?;
            let index = items.iter().position(|item| item.row.id == id)?;
            Some(ListOffset {
                item_ix: index + 1,
                offset_in_item: px((-anchor.y).max(0.)),
            })
        }
        None => Some(ListOffset {
            item_ix: 0,
            offset_in_item: px(position.offset.max(0.)),
        }),
    }
}

/// A closed channel's list with its measured rows.
pub struct Cached {
    pub list: ListState,
    pub items: Vec<Item>,
    /// What the timeline showed, to show again before the session answers.
    pub snapshot: std::sync::Arc<buzz_relay_client::TimelineSnapshot>,
}

#[derive(Default)]
struct Cache(VecDeque<(String, Cached)>);

impl Global for Cache {}

fn cache_key(community: &Entity<Community>, channel: &str, cx: &App) -> String {
    let partition = community.read(cx).partition();
    format!("{}\n{}\n{channel}", partition.origin, partition.viewer)
}

/// Takes the list kept for `channel`, if it is still cached.
pub fn take(community: &Entity<Community>, channel: &str, cx: &mut App) -> Option<Cached> {
    let key = cache_key(community, channel, cx);
    let cache = cx.default_global::<Cache>();
    let index = cache.0.iter().position(|(k, _)| *k == key)?;
    cache.0.remove(index).map(|(_, cached)| cached)
}

/// Keeps `cached` for `channel`, dropping the oldest beyond [`CACHED`].
pub fn keep(community: &Entity<Community>, channel: &str, cached: Cached, cx: &mut App) {
    let key = cache_key(community, channel, cx);
    let cache = cx.default_global::<Cache>();
    cache.0.retain(|(k, _)| *k != key);
    cache.0.push_back((key, cached));
    while cache.0.len() > CACHED {
        cache.0.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::markdown::tests::row;
    use core::prelude::v1::test;
    use std::sync::Arc;

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| {
                let mut row = row(&format!("m{i}"));
                row.id = EventId::from_byte_array([i as u8 + 1; 32]);
                Item {
                    row: Arc::new(row),
                    head: true,
                    day: None,
                    group: None,
                }
            })
            .collect()
    }

    #[test]
    fn restores_the_anchor_row_and_its_offset() {
        let items = items(3);
        let position = Position {
            offset: 0.,
            bottom: false,
            anchor: Some(Anchor {
                id: items[1].row.id.to_hex(),
                y: -14.,
            }),
        };
        let at = offset(&position, &items).unwrap();
        assert_eq!((at.item_ix, at.offset_in_item), (2, px(14.)));
        let json = serde_json::to_string(&position).unwrap();
        assert_eq!(serde_json::from_str::<Position>(&json).unwrap(), position);
    }

    #[test]
    fn stays_at_the_bottom_when_the_anchor_is_gone_or_the_reader_was_there() {
        let items = items(2);
        let gone = Position {
            offset: 0.,
            bottom: false,
            anchor: Some(Anchor {
                id: "f".repeat(64),
                y: 0.,
            }),
        };
        assert!(offset(&gone, &items).is_none());
        let bottom = Position {
            bottom: true,
            ..Position::default()
        };
        assert!(offset(&bottom, &items).is_none());
        let top = Position {
            offset: 20.,
            ..Position::default()
        };
        let at = offset(&top, &items).unwrap();
        assert_eq!((at.item_ix, at.offset_in_item), (0, px(20.)));
    }
}
