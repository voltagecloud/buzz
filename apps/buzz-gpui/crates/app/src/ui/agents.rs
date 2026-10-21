//! What agent views share: form layout, the Add host dialog, community
//! origins and the community's identity archives.

pub(crate) mod form;
mod host_dialog;
pub(crate) mod inventory;
pub(crate) mod relay;

pub(crate) use host_dialog::{AddHost, Dismissed};
