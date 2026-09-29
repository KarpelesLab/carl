//! Feature modules.
//!
//! Each module attaches a tool router to [`crate::server::Carl`]. They are
//! composed in [`crate::server::Carl::new`]. See `server.rs` for the recipe to
//! add a new feature.

pub mod email;
pub mod google;
pub mod system;
pub mod wallet;
