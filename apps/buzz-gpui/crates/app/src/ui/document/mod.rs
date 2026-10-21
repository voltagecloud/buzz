//! GitHub-flavoured Markdown documents, as GitHub shows an issue, pull
//! request or comment body (buzz-app's `GitHubBody.tsx`): single newlines
//! as breaks, raw HTML reduced to its text, images, links and media, and
//! every link resolved against the page it came from. Plugins draw one
//! with the `document` element and summarise one with
//! `document/preview`.

mod attachments;
pub mod body;
mod html;
pub mod preview;
pub mod view;
