//! EV-charger support.
//!
//! An EV charger is a controllable electrical flexible load (no thermal coupling). This module fuses
//! the available data sources into a single live [`state::EvState`] per charger — crucially
//! distinguishing "the car is on **our** wallbox" (the authoritative loxone signal) from "the car is
//! charging **somewhere**" (TeslaMate). The optimizer ([`crate::optimize::unified`]) schedules the
//! charge toward a target SoC by a deadline only while the car is controllable on our charger.

pub mod inputs;
pub mod prefs;
pub mod state;

pub use inputs::build_inputs;
pub use prefs::EvPreference;
