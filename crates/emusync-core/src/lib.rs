//! emusync core: data model, local state, and the sync engine (DESIGN §3–§6).
//!
//! ```no_run
//! use emusync_core::{ContainerDesc, Engine, EngineOptions};
//! use std::sync::Arc;
//!
//! let mut engine = Engine::open("data", EngineOptions::default())?;
//! let remote = emusync_remote::open(
//!     "s3://my-saves/emusync/v1?region=eu-west-1",
//!     Box::new(emusync_remote::EnvCredentials),
//! )?;
//! engine.add_remote(Arc::from(remote))?;
//! engine.add_container(ContainerDesc { location: "memcards/Mcd001.ps2".into(), ..Default::default() })?;
//! let report = engine.sync()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod chunk;
mod engine;
mod error;
pub mod fsutil;
pub mod hlc;
pub mod model;
mod store;

pub use engine::LOCKED;
pub use engine::{
    Applied, BlockKind, Blocked, BusyGate, CardStatus, ConflictInfo, ConflictPolicy, ContainerDesc, Engine,
    EngineOptions, HistoryEntry, RemoteSetup, Resolution, SyncReport, UnitStatus,
};
pub use error::{Error, Result};
pub use model::{short_id, split_unit, unit_id, CardMode, VersionRecord};
pub use store::ContainerRow;
