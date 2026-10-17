//! Authentication and authorization adapters for FigCraft Cloud.

pub mod authz;
pub mod embed_token;
pub mod oidc;
mod oidc_http;
pub mod password;
pub mod session_policy;
