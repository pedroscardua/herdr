//! Saved SSH hosts for the motionharvest client. No remote paths are local CWDs.
mod catalog;
pub(crate) mod ssh;

pub(crate) use catalog::{Catalog, Machine};
