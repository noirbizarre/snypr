//! Snypr — a GTK-based screenshot and annotation tool for Hyprland.
//!
//! This crate exposes the non-UI building blocks (capture, annotate model, output sinks, config,
//! IPC types) so they can be exercised from integration tests without spinning up GTK.

pub mod capture;
pub mod cli;
pub mod config;
pub mod context;
pub mod i18n;
pub mod ipc;
pub mod output;
pub mod path;
// Save-side plumbing for the annotation overlay. Lives outside `ui` because it touches no
// toolkit type, which keeps it covered by the no-`ui` build.
pub mod save;
pub mod wm;

#[cfg(feature = "ui")]
pub mod bridge;
#[cfg(feature = "ui")]
pub mod ui;

pub mod annotate;
pub mod daemon;

/// Fixtures shared across the in-crate test modules.
#[cfg(test)]
pub(crate) mod testing;

#[cfg(feature = "notify")]
pub mod notify;
