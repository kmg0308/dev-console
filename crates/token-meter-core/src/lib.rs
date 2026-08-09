//! TokenMeter domain and persistence code. This crate has no Tauri or React dependency.

mod atomic_file;

pub mod account;
pub mod account_service;
pub mod aggregation;
pub mod cache;
pub mod cleanup;
pub mod cleanup_archive;
pub mod dashboard;
pub mod hermes;
pub mod models;
pub mod parser;
pub mod scanner;
pub mod settings;
pub mod sync;
pub mod sync_store;

pub const CACHE_PARSER_VERSION: i64 = 4;
pub const SYNC_SCHEMA_VERSION: u32 = 2;
