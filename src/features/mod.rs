//! Feature modules.
//!
//! Each module attaches a tool router to [`crate::server::Manu`]. They are
//! composed in [`crate::server::Manu::new`]. See `server.rs` for the recipe to
//! add a new feature.

pub mod email;
pub mod system;
pub mod wallet;
