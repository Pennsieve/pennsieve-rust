// Copyright (c) 2018 Pennsieve, Inc. All Rights Reserved.

//! Pennsieve library top-level definitions go in this module.

pub mod api;
pub mod config;
pub mod error;
pub mod model;
pub mod types;
mod util;

// Re-export
pub use crate::ps::api::Pennsieve;
pub use crate::ps::config::{Config, Environment};
pub use crate::ps::types::{Error, ErrorKind, Future, Result, Stream};
