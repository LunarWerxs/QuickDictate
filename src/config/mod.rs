//! settings.json: its schema, its defaults, and how it is loaded and saved.

mod defaults;
mod query;
mod schema;
mod store;

#[cfg(test)]
mod tests;

pub use schema::{Config, EffectiveSettings, Profile};

/// The settings template, **baked into the exe** (no separate
/// settings.example.json shipped alongside). On first run — when no
/// settings.json exists — this is written out verbatim as the user's
/// settings.json, so they get a nicely-ordered, fully-populated file to edit.
pub const EXAMPLE_JSON: &str = include_str!("../../settings.example.json");
