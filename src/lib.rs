// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Tollgate - AI gateway and spend-control proxy for LLM providers.
//!
//! This crate exposes the in-process building blocks that the `tollgate`
//! binary composes: configuration, telemetry, the HTTP application, persistence,
//! the API-key, pricing, and budget engines, the provider adapters, and the CLI
//! dispatcher.

pub mod apikey;
pub mod app;
pub mod backends;
pub mod budget;
pub mod cli;
pub mod config;
pub mod console;
pub mod db;
pub mod demo;
pub mod error;
pub mod gateway;
pub mod pricing;
pub mod provider;
pub mod providers;
pub mod routes;
pub mod telemetry;
