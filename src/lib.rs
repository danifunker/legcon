//! legcon: connect to 1990s machines (docs/PLAN.md).
//!
//! Layers (CONTRIBUTING.md § Architecture policy): [`model`] owns machines, sessions and jobs;
//! [`controller`] owns what the user is doing; views (`src/cli`, `frontends/win32`) only render.
//! Views import [`controller`] alone; it re-exports the plain data they need.

pub mod cli;
pub mod controller;
pub mod model;
