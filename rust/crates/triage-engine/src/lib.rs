//! The triage learner's model, replay and self-tuning engine: pure, no I/O.
//!
//! A line-for-line port of the Python learner. Predictions from the same
//! state must be identical, so the code follows Python's evaluation order,
//! dictionary insertion order, CPython's compensated `sum()` and its
//! round-half-even rules (see [`num`]).

pub mod learner;
pub mod model;
pub mod num;
pub mod replay;
pub mod text;
pub mod tuning;

pub use learner::{Learner, Prediction};
pub use model::Model;
pub use replay::{Metrics, Record, TuneResult, fit_example, replay, self_tune};
pub use tuning::Tuning;
