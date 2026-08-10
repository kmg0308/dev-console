//! Runtime Atlas domain code. This crate has no Tauri or React dependency.

pub mod actions;
pub mod command;
pub mod git;
pub mod models;
pub mod observe;
pub mod relations;
pub mod repository;
pub mod runtime;
pub mod service;
pub mod sessions;
pub mod storage;

pub const STATUS_SCHEMA_VERSION: u32 = 2;
pub const CONFIGURATION_SCHEMA_VERSION: u32 = 5;
