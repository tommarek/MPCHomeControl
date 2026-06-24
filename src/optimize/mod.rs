//! Economic optimization for the house energy system.
//!
//! Decides how to dispatch flexible resources (battery now; heating and deferrable loads
//! later) against day-ahead electricity prices and the PV / consumption forecasts, to minimize
//! cost. The thermal MPC will plug into this layer as another flexible load.

pub mod battery;
pub mod config;
pub mod coordinator;
pub mod thermal;
pub mod unified;
