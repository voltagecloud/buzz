//! Workflows (`src/features/workflows`): channel-scoped YAML definitions
//! the relay runs. The session owns reads, commands and their results;
//! the bundled page owns drafts and the editor.

mod capability;
pub mod landing;
pub mod protocol;
mod types;
mod view;

pub use capability::{Definitions, DefinitionsSnapshot, Runs, Workflows};
pub use landing::{Landing, LandingSnapshot};
pub use types::*;
pub use view::{INTERRUPTED, READ_ERROR, ViewSnapshot, ViewStatus, WorkflowView};
