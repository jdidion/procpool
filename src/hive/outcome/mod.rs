mod batch;
mod iter;
#[allow(clippy::module_inception)]
mod outcome;
mod store;

pub use batch::OutcomeBatch;
pub use iter::OutcomeIteratorExt;
pub use outcome::Outcome;
pub use store::OutcomeStore;

pub(super) use store::sealed::{DerefOutcomes, OwnedOutcomes};
