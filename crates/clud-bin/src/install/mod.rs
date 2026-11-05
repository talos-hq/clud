//! `clud install <product>` (#2609): install another agent product and hand it
//! a working configuration.
//!
//! This is not `clud setup` (#2219, [`crate::setup_command`]) — that sets up
//! *clud*. This module installs a *product* (`hermes`, `talos`) by running the
//! product's own bootstrap, then offers to migrate clud's provider credentials
//! into the product's config. `clud setup <product>` is an exact alias; a bare
//! `clud setup` keeps its meaning.
//!
//! [`products`] holds product identity only (a table), [`command`] the steps.
//! See docs/architecture/install-products.md.

pub mod command;
pub mod configure;
pub mod products;
