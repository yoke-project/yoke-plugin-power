//! The host's power as a Yoke Plugin: UPower for the supply and the batteries, power-profiles-daemon
//! for the profile.
//!
//! `backend` talks to the two services and knows nothing of Yoke; `plugin` is the Plugin around it.

pub mod backend;
pub mod plugin;
