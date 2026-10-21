//! Components styled per section 6 of the spec.

mod activity;
mod avatar;
mod button;
mod checkbox;
mod chip;
mod dialog;
mod divider;
mod icon_button;
mod menu;
mod navigation_item;
mod panel_header;
mod popover;
mod radio;
mod search_field;
mod select;
mod slider;
mod switch;
mod tabs;
mod text_field;
mod thinking;
mod toast;
mod tooltip;

pub use activity::{Arrive, StackSize, arrive, avatar_stack, shimmer_text, typing_dots};
pub use avatar::{Avatar, AvatarShape, AvatarSize, Presence};
pub use button::{Button, ButtonShape, ButtonSize, ButtonVariant};
pub use checkbox::{Checkbox, CheckboxState};
pub use chip::{Badge, BadgeTone, InlineChip, Kbd};
pub use dialog::{AlertDialog, Dialog, DialogSize, Scrim};
pub use divider::Divider;
pub use icon_button::{IconButton, IconButtonShape, IconButtonSize};
pub use menu::{Menu, MenuItem, MenuLabel, MenuSeparator, MenuSize};
pub use navigation_item::{NavigationItem, NavigationItemVariant};
pub use panel_header::PanelHeader;
pub use popover::{Popover, PopoverPadding, PopoverWidth};
pub use radio::Radio;
pub use search_field::SearchField;
pub use select::Select;
pub use slider::Slider;
pub use switch::Switch;
pub use tabs::{Tab, TabVariant, Tabs};
pub use text_field::{TextArea, TextField};
pub use toast::{Toast, ToastTone};
pub use tooltip::Tooltip;

use gpui_kit::{App, ClickEvent, Hsla, Window, hsla};
use std::rc::Rc;

pub(crate) type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

pub(crate) const TRANSPARENT: Hsla = hsla(0., 0., 0., 0.);
