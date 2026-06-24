//! Forecasting models for the house-control system.
//!
//! Pure, IO-free predictive models built from historical data. Data *ingestion* (reading
//! the history out of InfluxDB) belongs in the data layer; these modules only consume
//! already-loaded samples, which keeps them deterministic and unit-testable.

pub mod calibration;
pub mod consumption;
pub mod solar;
