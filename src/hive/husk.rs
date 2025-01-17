use super::{
    Builder, Config, DerefOutcomes, Hive, Outcome, OutcomeBatch, OutcomeSender, OutcomeStore,
    OwnedOutcomes, SpawnError,
};
use crate::bee::{Queen, Worker};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

/// The remnants of a `Hive`.
pub struct Husk<W: Worker, Q: Queen<Kind = W>> {
    config: Config,
    queen: Q,
    num_panics: usize,
    outcomes: HashMap<usize, Outcome<W>>,
}

impl<W: Worker, Q: Queen<Kind = W>> Husk<W, Q> {
    pub(super) fn new(
        config: Config,
        queen: Q,
        num_panics: usize,
        outcomes: HashMap<usize, Outcome<W>>,
    ) -> Self {
        Self {
            config,
            queen,
            num_panics,
            outcomes,
        }
    }

    /// The `Queen` of the former `Hive`.
    pub fn queen(&self) -> &Q {
        &self.queen
    }

    /// The number of panicked threads in the former `Hive`.
    pub fn num_panics(&self) -> usize {
        self.num_panics
    }

    /// Consumes this `Husk` and returns the `Queen` and `Outcome`s.
    pub fn into_parts(self) -> (Q, OutcomeBatch<W>) {
        (self.queen, OutcomeBatch::new(self.outcomes))
    }

    /// Returns a new `Builder` that will create a `Hive` with the same configuration as the one
    /// that produced this `Husk`.
    pub fn as_builder(&self) -> Builder {
        self.config.clone().into()
    }

    /// Consumes this `Husk` and returns a new `Hive` with the same configuration as the one that
    /// produced this `Husk`.
    pub fn into_hive(self) -> Result<Hive<W, Q>, SpawnError> {
        self.as_builder().build(self.queen)
    }

    fn collect_unprocessed(outcomes: HashMap<usize, Outcome<W>>) -> Vec<W::Input> {
        outcomes
            .into_values()
            .filter_map(|outcome| match outcome {
                Outcome::Unprocessed { input: value, .. } => Some(value),
                _ => None,
            })
            .collect()
    }

    /// Consumes this `Husk` and creates a new `Hive` with the same configuration as the one that
    /// produced this `Husk`, and queues all the `Outcome::Unprocessed` values. The results will
    /// be sent to `tx`. Returns the new `Hive` and the indices of the tasks that were queued.
    ///
    /// This method panics if there is an error creating the new `Hive`.
    pub fn into_hive_swarm_unprocessed_to(self, tx: OutcomeSender<W>) -> (Hive<W, Q>, Vec<usize>) {
        let hive = self.as_builder().build(self.queen).unwrap();
        let unprocessed = Self::collect_unprocessed(self.outcomes);
        let indices = hive.swarm_send(unprocessed, tx);
        (hive, indices)
    }

    /// Consumes this `Husk` and creates a new `Hive` with the same configuration as the one that
    /// produced this `Husk`, and queues all the `Outcome::Unprocessed` values. The results will
    /// be retained in the new `Hive` for later retrieval. Returns the new `Hive` and the indices
    /// of the tasks that were queued.
    ///
    /// This method panics if there is an error creating the new `Hive`.
    pub fn into_hive_swarm_unprocessed_store(self) -> (Hive<W, Q>, Vec<usize>) {
        let hive = self.as_builder().build(self.queen).unwrap();
        let unprocessed = Self::collect_unprocessed(self.outcomes);
        let indices = hive.swarm_store(unprocessed);
        (hive, indices)
    }
}

impl<W: Worker, Q: Queen<Kind = W>> DerefOutcomes<W> for Husk<W, Q> {
    fn outcomes_deref(&self) -> impl Deref<Target = HashMap<usize, Outcome<W>>> {
        &self.outcomes
    }

    fn outcomes_deref_mut(&mut self) -> impl DerefMut<Target = HashMap<usize, Outcome<W>>> {
        &mut self.outcomes
    }
}

impl<W: Worker, Q: Queen<Kind = W>> OwnedOutcomes<W> for Husk<W, Q> {
    fn outcomes(self) -> HashMap<usize, Outcome<W>> {
        self.outcomes
    }

    fn outcomes_ref(&self) -> &HashMap<usize, Outcome<W>> {
        &self.outcomes
    }
}

impl<W: Worker, Q: Queen<Kind = W>> OutcomeStore<W> for Husk<W, Q> {}

#[cfg(test)]
mod tests {
    use crate::bee::stock::{PunkWorker, Thunk, ThunkWorker};
    use crate::hive::{outcome_channel, Builder, Outcome, OutcomeIteratorExt, OutcomeStore};

    #[test]
    fn test_unprocessed() {
        // don't spin up any worker threads so that no tasks will be processed
        let hive = Builder::new()
            .num_threads(0)
            .build_with_default::<ThunkWorker<u8>>()
            .unwrap();
        let mut indices = hive.map_store((0..10).map(|i| Thunk::of(move || i)));
        // cancel and smash the hive before the tasks can be processed
        hive.suspend();
        let mut husk = hive.try_into_husk().unwrap();
        assert!(husk.has_unprocessed());
        for i in indices.iter() {
            assert!(husk.get(*i).unwrap().is_unprocessed());
        }
        assert_eq!(husk.iter_unprocessed().count(), 10);
        let mut unprocessed_indices = husk
            .iter_unprocessed()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        indices.sort();
        unprocessed_indices.sort();
        assert_eq!(indices, unprocessed_indices);
        assert_eq!(husk.remove_all_unprocessed().len(), 10);
    }

    #[test]
    fn test_reprocess_unprocessed() {
        // don't spin up any worker threads so that no tasks will be processed
        let hive1 = Builder::new()
            .num_threads(0)
            .build_with_default::<ThunkWorker<u8>>()
            .unwrap();
        let _ = hive1.map_store((0..10).map(|i| Thunk::of(move || i)));
        // cancel and smash the hive before the tasks can be processed
        hive1.suspend();
        let husk1 = hive1.try_into_husk().unwrap();
        let (hive2, _) = husk1.into_hive_swarm_unprocessed_store();
        // now spin up worker threads to process the tasks
        hive2.grow(8);
        hive2.join();
        let husk2 = hive2.try_into_husk().unwrap();
        assert!(!husk2.has_unprocessed());
        assert!(husk2.has_successes());
        assert_eq!(husk2.iter_successes().count(), 10);
    }

    #[test]
    fn test_reprocess_unprocessed_to() {
        // don't spin up any worker threads so that no tasks will be processed
        let hive1 = Builder::new()
            .num_threads(0)
            .build_with_default::<ThunkWorker<u8>>()
            .unwrap();
        let _ = hive1.map_store((0..10).map(|i| Thunk::of(move || i)));
        // cancel and smash the hive before the tasks can be processed
        hive1.suspend();
        let husk1 = hive1.try_into_husk().unwrap();
        let (tx, rx) = outcome_channel();
        let (hive2, indices) = husk1.into_hive_swarm_unprocessed_to(tx);
        // now spin up worker threads to process the tasks
        hive2.grow(8);
        hive2.join();
        let husk2 = hive2.try_into_husk().unwrap();
        assert!(husk2.is_empty());
        let mut outputs = rx
            .take_ordered(indices)
            .map(Outcome::unwrap)
            .collect::<Vec<_>>();
        outputs.sort();
        assert_eq!(outputs, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_into_result() {
        let hive = Builder::new()
            .num_threads(4)
            .build_with_default::<ThunkWorker<u8>>()
            .unwrap();
        hive.map_store((0..10).map(|i| Thunk::of(move || i)));
        hive.join();
        let mut outputs = hive.try_into_husk().unwrap().into_parts().1.unwrap();
        outputs.sort();
        assert_eq!(outputs, (0..10).collect::<Vec<_>>());
    }

    #[test]
    #[should_panic]
    fn test_into_result_panic() {
        let hive = Builder::new()
            .num_threads(4)
            .build_with_default::<PunkWorker<u8>>()
            .unwrap();
        hive.map_store(
            (0..10).map(|i| Thunk::of(move || if i == 5 { panic!("oh no!") } else { i })),
        );
        hive.join();
        let (_, result) = hive.try_into_husk().unwrap().into_parts();
        let _ = result.ok_or_unwrap_errors(true);
    }
}
