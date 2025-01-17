use super::counter::{self, DualCounter};
use super::{Config, Outcome, OutcomeSender, Shared, Task, TaskReceiver};
use crate::atomic::{Atomic, AtomicInt, AtomicUsize};
use crate::bee::{Context, Queen, Worker};
use crate::channel::SenderExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ops::DerefMut;
use std::thread::Builder;
use std::time::Duration;
use std::{fmt, iter, mem};

impl<W: Worker, Q: Queen<Kind = W>> Shared<W, Q> {
    pub fn new(config: Config, queen: Q, task_rx: TaskReceiver<W>) -> Self {
        Shared {
            config,
            queen: Mutex::new(queen),
            task_rx: Mutex::new(task_rx),
            num_tasks: DualCounter::default(),
            next_task_index: Default::default(),
            num_panics: Default::default(),
            num_referrers: AtomicUsize::new(1),
            poisoned: Default::default(),
            suspended: Default::default(),
            resume_gate: Default::default(),
            join_gate: Default::default(),
            outcomes: Default::default(),
            #[cfg(feature = "retry")]
            retry_queue: Default::default(),
            #[cfg(feature = "retry")]
            next_retry: Default::default(),
        }
    }

    /// Returns a `Builder` for creating a new thread in the `Hive`.
    pub fn thread_builder(&self) -> Builder {
        let mut builder = Builder::new();
        if let Some(ref name) = self.config.thread_name.get() {
            builder = builder.name(name.clone());
        }
        if let Some(ref stack_size) = self.config.thread_stack_size.get() {
            builder = builder.stack_size(stack_size.to_owned());
        }
        builder
    }

    /// Increases the maximum number of threads allowed in the `Hive` by `num_threads` and returns
    /// the previous value.
    pub fn add_threads(&self, num_threads: usize) -> usize {
        self.config.num_threads.add(num_threads).unwrap()
    }

    /// Ensures that the number of threads is at least `num_threads`. Returns the previous value.
    pub fn ensure_threads(&self, num_threads: usize) -> usize {
        self.config.num_threads.set_max(num_threads).unwrap()
    }

    /// Returns a new `Worker` from the queen, or an error if a `Worker` could not be created.
    pub fn create_worker(&self) -> Q::Kind {
        self.queen.lock().create()
    }

    /// Increments the number of queued tasks. Returns a new `Task` with the provided input and
    /// `outcome_tx` and the next index.
    pub fn prepare_task(&self, input: W::Input, outcome_tx: Option<OutcomeSender<W>>) -> Task<W> {
        self.num_tasks
            .increment_left(1)
            .expect("overflowed queued task counter");
        let index = self.next_task_index.add(1);
        let ctx = Context::new(index, self.suspended.clone());
        Task::new(input, ctx, outcome_tx)
    }

    /// Increments the number of queued tasks by the number of provided inputs. Returns an iterator
    /// over `Task`s created from the provided inputs, `outcome_tx`s, and sequential indices.
    pub fn prepare_batch<'a, T: Iterator<Item = W::Input> + 'a>(
        &'a self,
        min_size: usize,
        inputs: T,
        outcome_tx: Option<OutcomeSender<W>>,
    ) -> impl Iterator<Item = Task<W>> + 'a {
        self.num_tasks
            .increment_left(min_size as u64)
            .expect("overflowed queued task counter");
        let index_start = self.next_task_index.add(min_size);
        let index_end = index_start + min_size;
        inputs
            .map(Some)
            .chain(iter::repeat_with(|| None))
            .zip(
                (index_start..index_end)
                    .map(Some)
                    .chain(iter::repeat_with(|| None)),
            )
            .map_while(move |pair| match pair {
                (Some(input), Some(index)) => Some(Task {
                    input,
                    ctx: Context::new(index, self.suspended.clone()),
                    //attempt: 0,
                    outcome_tx: outcome_tx.clone(),
                }),
                (Some(input), None) => Some(self.prepare_task(input, outcome_tx.clone())),
                (None, Some(_)) => panic!("batch contained fewer than {min_size} items"),
                (None, None) => None,
            })
    }

    /// Sends an outcome to `outcome_tx`, or stores it in the `Hive` shared data if there is no
    /// sender, or if the send fails.
    pub fn send_or_store_outcome(&self, outcome: Outcome<W>, outcome_tx: Option<OutcomeSender<W>>) {
        if let Some(outcome) = if let Some(tx) = outcome_tx {
            tx.try_send_msg(outcome)
        } else {
            Some(outcome)
        } {
            self.add_outcome(outcome)
        }
    }

    /// Converts each `Task` in the iterator into `Outcome::Unprocessed` and attempts to send it
    /// to its `OutcomeSender` if there is one, or stores it if there is no sender or the send
    /// fails. Returns a vector of indices of the tasks.
    pub fn send_or_store_as_unprocessed<I>(&self, tasks: I) -> Vec<usize>
    where
        I: Iterator<Item = Task<W>>,
    {
        // don't unlock outcomes unless we have to
        let mut outcomes = Option::None;
        tasks
            .map(|task| {
                let index = task.index();
                if let Some(outcome) = task.into_unprocessed_try_send() {
                    outcomes
                        .get_or_insert_with(|| self.outcomes.lock())
                        .insert(index, outcome);
                }
                index
            })
            .collect()
    }

    /// Called by a worker thread after completing a task. Notifies any thread that has `join`ed
    /// the `Hive` if there is no more work to be done.
    pub fn finish_task(&self, panicking: bool) {
        self.num_tasks
            .decrement_right(1)
            .expect("active task counter was smaller than expected");
        if panicking {
            self.num_panics.add(1);
        }
        self.no_work_notify_all();
    }

    /// Returns a tuple with the number of (queued, active) tasks.
    #[inline]
    pub fn num_tasks(&self) -> (u64, u64) {
        self.num_tasks.get()
    }

    /// Returns `true` if the hive has not been poisoned and there are either active tasks or there
    /// are queued tasks and the cancelled flag hasn't been set.
    #[inline]
    pub fn has_work(&self) -> bool {
        !self.is_poisoned() && {
            let (queued, active) = self.num_tasks();
            active > 0 || (!self.is_suspended() && queued > 0)
        }
    }

    /// Blocks the current thread until all active tasks have been processed. Also waits until all
    /// queued tasks have been processed unless the suspended flag has been set.
    pub fn wait_on_done(&self) {
        self.join_gate.wait_while(|| self.has_work());
    }

    /// Notify all observers joining this hive when there is no more work to do.
    pub fn no_work_notify_all(&self) {
        if !self.has_work() {
            self.join_gate.notify_all();
        }
    }

    /// Returns the number of `Hive`s holding a reference to this shared data.
    pub fn num_referrers(&self) -> usize {
        self.num_referrers.get()
    }

    /// Increments the number of referrers and returns the previous value.
    pub fn referrer_is_cloning(&self) -> usize {
        self.num_referrers.add(1)
    }

    /// Decrements the number of referrers and returns the previous value.
    pub fn referrer_is_dropping(&self) -> usize {
        self.num_referrers.sub(1)
    }

    /// Sets the `poisoned` flag to `true`. Converts all queued tasks to `Outcome::Unprocessed`
    /// and stores them in `outcomes`.
    pub fn poison(&self) {
        self.poisoned.set(true);
        self.drain_tasks_into_unprocessed();
    }

    /// Returns `true` if the hive has been poisoned. A poisoned have may accept new tasks but will
    /// never process them. Unprocessed tasks can be retrieved by calling `take_outcomes` or
    /// `try_into_husk`.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// Sets the `cancelled` flag. Worker threads may terminate early. No new worker threads will
    /// be spawned. Returns `true` if the value was changed.
    pub fn set_suspended(&self, suspended: bool) -> bool {
        if self.suspended.set(suspended) == suspended {
            false
        } else {
            if !suspended {
                self.resume_gate.notify_all();
            }
            true
        }
    }

    /// Returns `true` if the `suspended` flag has been set.
    #[inline]
    pub fn is_suspended(&self) -> bool {
        self.suspended.get()
    }

    /// Returns a mutable reference to the retained task outcomes.
    pub fn outcomes(&self) -> impl DerefMut<Target = HashMap<usize, Outcome<W>>> + '_ {
        self.outcomes.lock()
    }

    /// Adds a new outcome to the retained task outcomes.
    pub fn add_outcome(&self, outcome: Outcome<W>) {
        let mut lock = self.outcomes.lock();
        lock.insert(*outcome.index(), outcome);
    }

    /// Removes and returns all retained task outcomes.
    pub fn take_outcomes(&self) -> HashMap<usize, Outcome<W>> {
        let mut lock = self.outcomes.lock();
        mem::take(&mut *lock)
    }

    /// Removes and returns all retained `Unprocessed` outcomes.
    pub fn take_unprocessed(&self) -> Vec<Outcome<W>> {
        let mut outcomes = self.outcomes.lock();
        let unprocessed_indices: Vec<_> = outcomes
            .keys()
            .cloned()
            .filter(|index| matches!(outcomes.get(index), Some(Outcome::Unprocessed { .. })))
            .collect();
        unprocessed_indices
            .into_iter()
            .map(|index| outcomes.remove(&index).unwrap())
            .collect()
    }
}

impl<W: Worker, Q: Queen<Kind = W>> fmt::Debug for Shared<W, Q> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (queued, active) = self.num_tasks();
        f.debug_struct("Shared")
            .field("name", &self.config.thread_name)
            .field("num_threads", &self.config.num_threads)
            .field("num_tasks_queued", &queued)
            .field("num_tasks_active", &active)
            .finish()
    }
}

#[cfg(feature = "affinity")]
mod affinity {
    use crate::bee::{Queen, Worker};
    use crate::hive::cores::{Core, Cores};
    use crate::hive::Shared;

    impl<W: Worker, Q: Queen<Kind = W>> Shared<W, Q> {
        /// Adds cores to which worker threads may be pinned.
        pub fn add_core_affinity(&self, new_cores: &Cores) {
            let _ = self.config.affinity.try_update_with(|mut affinity| {
                let updated = affinity.union(new_cores) > 0;
                updated.then_some(affinity)
            });
        }

        /// Returns the `Core` to which the specified worker thread may be pinned, if any.
        pub fn get_core_affinity(&self, thread_index: usize) -> Option<Core> {
            self.config
                .affinity
                .get()
                .and_then(|cores| cores.get(thread_index))
        }
    }
}

// time to wait in between polling the retry queue and then the task receiver
const RECV_TIMEOUT: Duration = Duration::from_secs(1);

// TODO: if `outcomes` were `DerefMut` then the argument could either be a mutable referece or
// a Lazy<Mutex> that aquires the lock on first access. Unfortunately, rust's Lazy does not support
// mutable access, so we'd need something like OnceCell or OnceMutex.
fn send_or_store<W: Worker, I: Iterator<Item = Task<W>>>(
    tasks: I,
    outcomes: &mut HashMap<usize, Outcome<W>>,
) {
    tasks.for_each(|task| {
        if let Some(outcome) = task.into_unprocessed_try_send() {
            outcomes.insert(*outcome.index(), outcome);
        }
    });
}

#[derive(thiserror::Error, Debug)]
pub enum NextTaskError {
    #[error("Task receiver disconnected")]
    Disconnected,
    #[error("The hive has been poisoned")]
    Poisoned,
    #[error("Task counter has invalid state")]
    InvalidCounter(counter::CounterError),
}

#[cfg(not(feature = "retry"))]
mod no_retry {
    use super::{send_or_store, NextTaskError};
    use crate::atomic::Atomic;
    use crate::bee::{Queen, Worker};
    use crate::hive::{Husk, Shared, Task};
    use std::sync::mpsc::RecvTimeoutError;

    impl<W: Worker, Q: Queen<Kind = W>> Shared<W, Q> {
        /// Returns the next queued `Task`. The thread blocks until a new task becomes available, and
        /// since this requires holding a lock on the task `Reciever`, this also blocks any other
        /// threads that call this method. Returns `None` if the task `Sender` has hung up and there
        /// are no tasks queued. Also returns `None` if the cancelled flag has been set.
        pub fn next_task(&self) -> Result<Task<W>, NextTaskError> {
            loop {
                self.resume_gate.wait_while(|| self.is_suspended());

                if self.is_poisoned() {
                    return Err(NextTaskError::Poisoned);
                }

                match self.task_rx.lock().recv_timeout(super::RECV_TIMEOUT) {
                    Ok(task) => break Ok(task),
                    Err(RecvTimeoutError::Disconnected) => break Err(NextTaskError::Disconnected),
                    Err(RecvTimeoutError::Timeout) => continue,
                }
            }
            .and_then(|task| match self.num_tasks.transfer(1) {
                Ok(_) => Ok(task),
                Err(e) => {
                    // poison the hive so it can't be used anymore
                    self.poison();
                    Err(NextTaskError::InvalidCounter(e))
                }
            })
        }

        /// Drains all queued tasks, converts them into `Outcome::Unprocessed` outcomes, and tries
        /// to send them or (if the task does not have a sender, or if the send fails) stores them
        /// in the `outcomes` map.
        pub fn drain_tasks_into_unprocessed(&self) {
            let task_rx = self.task_rx.lock();
            let mut outcomes = self.outcomes.lock();
            send_or_store(task_rx.try_iter(), &mut outcomes);
        }

        /// Consumes this `Shared` and returns a `Husk` containing the `Queen`, panic count, stored
        /// outcomes, and all configuration information necessary to create a new `Hive`. Any queued
        /// tasks are converted into `Outcome::Unprocessed` outcomes and either sent to the task's
        /// sender or (if there is no sender, or the send fails) stored in the `outcomes` map.
        pub fn try_into_husk(self) -> Husk<W, Q> {
            let task_rx = self.task_rx.into_inner();
            let mut outcomes = self.outcomes.into_inner();
            send_or_store(task_rx.try_iter(), &mut outcomes);
            Husk::new(
                self.config.into_unsync(),
                self.queen.into_inner(),
                self.num_panics.into_inner(),
                outcomes,
            )
        }
    }
}

#[cfg(feature = "retry")]
mod retry {
    use super::NextTaskError;
    use crate::atomic::Atomic;
    use crate::bee::{Context, Queen, Worker};
    use crate::hive::{Husk, OutcomeSender, Shared, Task};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    impl<W: Worker, Q: Queen<Kind = W>> Shared<W, Q> {
        /// Returns `true` if the hive is configured to retry tasks.
        pub fn can_retry(&self, ctx: &Context) -> bool {
            self.config
                .max_retries
                .get()
                .map(|max_retries| ctx.attempt() < max_retries)
                .unwrap_or(false)
        }

        fn update_next_retry(&self, instant: Option<Instant>) {
            let mut next_retry = self.next_retry.write();
            if let Some(new_val) = instant {
                if next_retry.map(|cur_val| new_val < cur_val).unwrap_or(true) {
                    next_retry.replace(new_val);
                }
            } else {
                next_retry.take();
            }
        }

        pub fn queue_retry(
            &self,
            input: W::Input,
            ctx: Context,
            outcome_tx: Option<OutcomeSender<W>>,
        ) {
            let delay = self
                .config
                .retry_factor
                .get()
                .map(|retry_factor| {
                    2u64.checked_pow(ctx.attempt() - 1)
                        .and_then(|multiplier| {
                            retry_factor
                                .checked_mul(multiplier)
                                .or(Some(u64::MAX))
                                .map(Duration::from_nanos)
                        })
                        .unwrap()
                })
                .unwrap_or_default();
            let task = Task::new(input, ctx, outcome_tx);
            let mut queue = self.retry_queue.lock();
            self.num_tasks
                .increment_left(1)
                .expect("overflowed queued task counter");
            let available_at = queue.push(task, delay);
            self.update_next_retry(Some(available_at));
        }

        /// Returns the next queued `Task`. The thread blocks until a new task becomes available, and
        /// since this requires holding a lock on the task `Reciever`, this also blocks any other
        /// threads that call this method. Returns `None` if the task `Sender` has hung up and there
        /// are no tasks queued for retry.
        pub fn next_task(&self) -> Result<Task<W>, NextTaskError> {
            loop {
                self.resume_gate.wait_while(|| self.is_suspended());

                if self.is_poisoned() {
                    return Err(NextTaskError::Poisoned);
                }

                let has_retry = {
                    let next_retry = self.next_retry.read();
                    next_retry.is_some_and(|next_retry| next_retry <= Instant::now())
                };
                if has_retry {
                    let mut queue = self.retry_queue.lock();
                    if let Some(task) = queue.try_pop() {
                        self.update_next_retry(queue.next_available());
                        break Ok(task);
                    }
                }

                match self.task_rx.lock().recv_timeout(super::RECV_TIMEOUT) {
                    Ok(task) => break Ok(task),
                    Err(RecvTimeoutError::Disconnected) => break Err(NextTaskError::Disconnected),
                    Err(RecvTimeoutError::Timeout) => continue,
                }
            }
            .and_then(|task| match self.num_tasks.transfer(1) {
                Ok(_) => Ok(task),
                Err(e) => Err(NextTaskError::InvalidCounter(e)),
            })
        }

        /// Drains all queued tasks, converts them into `Outcome::Unprocessed` outcomes, and tries
        /// to send them or (if the task does not have a sender, or if the send fails) stores them
        /// in the `outcomes` map.
        pub fn drain_tasks_into_unprocessed(&self) {
            let mut outcomes = self.outcomes.lock();
            let task_rx = self.task_rx.lock();
            super::send_or_store(task_rx.try_iter(), &mut outcomes);
            let mut retry_queue = self.retry_queue.lock();
            super::send_or_store(retry_queue.drain(), &mut outcomes);
        }

        /// Consumes this `Shared` and returns a `Husk` containing the `Queen`, panic count, stored
        /// outcomes, and all configuration information necessary to create a new `Hive`. Any queued
        /// tasks are converted into `Outcome::Unprocessed` outcomes and either sent to the task's
        /// sender or (if there is no sender, or the send fails) stored in the `outcomes` map.
        pub fn try_into_husk(self) -> Husk<W, Q> {
            let mut outcomes = self.outcomes.into_inner();
            let task_rx = self.task_rx.into_inner();
            super::send_or_store(task_rx.try_iter(), &mut outcomes);
            let mut retry_queue = self.retry_queue.into_inner();
            super::send_or_store(retry_queue.drain(), &mut outcomes);
            Husk::new(
                self.config.into_unsync(),
                self.queen.into_inner(),
                self.num_panics.into_inner(),
                outcomes,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bee::stock::ThunkWorker;
    use crate::bee::DefaultQueen;

    type VoidThunkWorker = ThunkWorker<()>;
    type VoidThunkWorkerShared = super::Shared<VoidThunkWorker, DefaultQueen<VoidThunkWorker>>;

    #[test]
    fn test_sync_shared() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<VoidThunkWorkerShared>();
    }

    #[test]
    fn test_send_shared() {
        fn assert_send<T: Send>() {}
        assert_send::<VoidThunkWorkerShared>();
    }
}
