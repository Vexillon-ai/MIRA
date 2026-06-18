// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin packages — signed, multi-component bundles (see
//! design-docs/plugin-packages.md).
//!
//! A package is the distribution + trust unit; its components register into
//! the existing subsystems (skills store, MCP host, channel accounts). The
//! package layer is install/uninstall/trust orchestration that fans out — it
//! does not merge those runtime registries.
//!
//!(this slice): the manifest types, verification, and trust levels —
//! "MIRA can verify a package and show its trust level." Later phases add the
//! bundle store, the install/update/teardown engine, and the per-component
//! installers (`mcp_server`).

pub mod bundle;
pub mod container;
pub mod engine;
pub mod host;
pub mod install;
pub mod launcher;
pub mod manifest;
pub mod service;
pub mod session_store;
pub mod store;
pub mod update;
pub mod verify;
pub mod wizard;

pub use bundle::{parse_bundle, ParsedBundle, MANIFEST_NAME};
pub use install::{
    gate_install, install_package, reverse_ledger, uninstall_package, InstallOutcome,
};
pub use store::{InstalledPackage, Ledger, LedgerEntry, NewInstall, PackageStore};
pub use manifest::{
    Capabilities, Component, ComponentKind, ManifestError, PackageManifest, Runtime,
    PACKAGE_FORMAT,
};
pub use verify::{sign_package, verify_package, TrustLevel};
pub use update::{apply_migrations, plan_update, policy_check, UpdateBlock, UpdatePlan};
pub use wizard::{
    Action, Actor, ConfigField, ConfigMigration, Encoding, FieldSource, FieldType, GenerateSpec,
    OnFail, RunOn, SetupStep, VerifyProbe,
};
