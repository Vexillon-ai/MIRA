// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/mod.rs

pub mod capabilities;
pub mod groups;
pub mod identities;
pub mod invites;
pub mod ldap;
pub mod oidc;
pub mod local;
pub mod middleware;
pub mod models;
pub mod tokens;

pub use capabilities::CapabilityProfile;
pub use groups::{Group, NewGroup, UpdateGroup};
pub use models::{AuthDb, User, Role, NewUser, UserProfile};
pub use middleware::{AuthUser, AdminUser};
pub use local::LocalAuthService;
pub use tokens::TokenPair;
