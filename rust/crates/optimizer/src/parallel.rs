use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

// Small RF workloads cannot amortize the cost of starting and coordinating many workers.
const AUTO_PAIRS_PER_WORKER: usize = 256;
const BATCH_SIZE: usize = 16;

pub(crate) fn worker_count(requested: usize, len: usize) -> usize {
    if requested == 0 {
        let useful_workers = len.div_ceil(AUTO_PAIRS_PER_WORKER);
        if useful_workers <= 1 {
            return 1;
        }
        return thread::available_parallelism()
            .map_or(1, |cpus| cpus.get().saturating_sub(1).max(1))
            .min(useful_workers);
    }
    requested.min(len)
}

/// For uncancellable work the caller can admit batches instead of sending each
/// item individually. Results and errors are still consumed in input order.
pub(crate) fn for_each_batched<T: Copy + Send + Sync, R: Send, E: Send>(
    items: &[T],
    workers: usize,
    cancelled: Option<&dyn Fn() -> bool>,
    evaluate: impl Fn(T, &dyn Fn() -> bool) -> Result<R, E> + Sync,
    mut accept: impl FnMut(T, R),
) -> Result<(), E> {
    if let Some(cancelled) = cancelled {
        return for_each_ordered(items, workers, cancelled, evaluate, accept);
    }
    if workers <= 1 || items.is_empty() {
        return for_each_ordered(items, 1, &|| false, evaluate, accept);
    }
    let batch_size = BATCH_SIZE.min(items.len().div_ceil(workers));
    let batches: Vec<_> = items.chunks(batch_size).collect();
    let error = RefCell::new(None);
    for_each_ordered(
        &batches,
        workers.min(batches.len()),
        &|| error.borrow().is_some(),
        |batch, cancelled| {
            let mut results = Vec::with_capacity(batch.len());
            for &item in batch {
                if cancelled() {
                    break;
                }
                match evaluate(item, cancelled) {
                    Ok(result) => results.push(result),
                    Err(error) => return Ok::<_, E>((results, Some(error))),
                }
            }
            Ok((results, None))
        },
        |batch, (results, failure)| {
            for (&item, result) in batch.iter().zip(results) {
                accept(item, result);
            }
            error.replace(failure);
        },
    )?;
    error.into_inner().map_or(Ok(()), Err)
}

struct StopWorkers<'a>(&'a AtomicBool);

impl Drop for StopWorkers<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Keep one outstanding item per worker. The caller consumes results, runs hooks, and
/// admits new work in input order while workers can finish sampling in any order.
fn for_each_ordered<T: Copy + Send, R: Send, E>(
    items: &[T],
    workers: usize,
    cancelled: &dyn Fn() -> bool,
    evaluate: impl Fn(T, &dyn Fn() -> bool) -> Result<R, E> + Sync,
    mut accept: impl FnMut(T, R),
) -> Result<(), E>
where
    E: Send,
{
    let serial = |accept: &mut dyn FnMut(T, R)| {
        for &item in items {
            if cancelled() {
                break;
            }
            let result = evaluate(item, cancelled);
            if cancelled() {
                break;
            }
            accept(item, result?);
        }
        Ok(())
    };
    if workers <= 1 {
        return serial(&mut accept);
    }

    let stop = AtomicBool::new(false);
    let worker_cancelled = || stop.load(Ordering::Relaxed);
    thread::scope(|scope| {
        let _stop_workers = StopWorkers(&stop);
        let mut remaining = items.iter().copied();
        let mut active = VecDeque::new();
        for _ in 0..workers {
            if cancelled() {
                break;
            }
            let (send, tasks) = mpsc::sync_channel(1);
            let (results, receive) = mpsc::sync_channel(1);
            let evaluate = &evaluate;
            let spawned = thread::Builder::new().spawn_scoped(scope, move || {
                while let Ok(item) = tasks.recv() {
                    if worker_cancelled()
                        || results.send(evaluate(item, &worker_cancelled)).is_err()
                    {
                        break;
                    }
                }
            });
            if spawned.is_err() {
                break;
            }
            let item = remaining.next().expect("at most one worker per item");
            send.send(item).expect("worker accepts its first item");
            active.push_back((item, send, receive));
        }
        if active.is_empty() {
            return serial(&mut accept);
        }

        while let Some((item, send, receive)) = active.pop_front() {
            let result = loop {
                if cancelled() {
                    return Ok(());
                }
                match receive.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => break result,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => {
                        // The scope propagates a panic from the worker.
                        return Ok(());
                    }
                }
            };
            if cancelled() {
                return Ok(());
            }
            accept(item, result?);
            if cancelled() {
                return Ok(());
            }
            if let Some(next) = remaining.next() {
                send.send(next).expect("worker accepts the next item");
                active.push_back((next, send, receive));
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    #[test]
    fn accepts_results_in_input_order_even_if_a_later_item_completes_first() {
        let finished = Mutex::new(Vec::new());
        let changed = Condvar::new();
        let mut accepted = Vec::new();
        for_each_ordered(
            &[0, 1],
            2,
            &|| false,
            |item, _| {
                let mut finished = finished.lock().unwrap();
                if item == 0 {
                    (finished, _) = changed
                        .wait_timeout_while(finished, Duration::from_secs(5), |v| v.is_empty())
                        .unwrap();
                    assert_eq!(*finished, [1]);
                }
                finished.push(item);
                changed.notify_one();
                Ok::<_, ()>(item * 2)
            },
            |item, result| accepted.push((item, result)),
        )
        .unwrap();
        assert_eq!(*finished.lock().unwrap(), [1, 0]);
        assert_eq!(accepted, [(0, 0), (1, 2)]);
    }

    #[test]
    fn batched_errors_accept_only_the_input_prefix_before_the_first_error() {
        let items: Vec<_> = (0..64).collect();
        let mut accepted = Vec::new();
        let outcome = for_each_batched(
            &items,
            4,
            None,
            |item, _| match item {
                5 | 17 => Err(item),
                _ => Ok(item),
            },
            |_, result| accepted.push(result),
        );
        assert_eq!(outcome, Err(5));
        assert_eq!(accepted, [0, 1, 2, 3, 4]);
    }
}
