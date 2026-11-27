//! Chrome drawn around the remote screen.
//!
//! Ratatui widgets share a cell buffer for the status line, menu and notifications.
//! Their explicit background colours cover the desktop tiles underneath. The diff
//! writes only the cells that changed.

pub mod chrome;
pub mod menu;
mod paint;
pub mod status;
pub mod theme;
pub mod toast;

/// The button that takes a piece of chrome off the screen: the menu's title carries one
/// and so does the notification popup. One string for both, because they are the same
/// control and two copies of it could come to disagree about what it looks like.
pub const CLOSE: &str = "x";
