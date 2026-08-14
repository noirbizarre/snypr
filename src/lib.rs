//! Snypr — a GTK-based screenshot and annotation tool for Hyprland.
//!
//! This crate exposes the non-UI building blocks (capture, annotate model, output sinks, config,
//! IPC types) so they can be exercised from integration tests without spinning up GTK.

pub mod capture;
pub mod cli;
pub mod config;
pub mod context;
pub mod hypr;
pub mod i18n;
pub mod ipc;
pub mod output;
pub mod path;

#[cfg(feature = "ui")]
pub mod bridge;
#[cfg(feature = "ui")]
pub mod ui;

pub mod annotate;
pub mod daemon;

#[cfg(feature = "notify")]
pub mod notify;
