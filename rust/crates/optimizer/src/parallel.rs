use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

const MIN_AUTO_PARALLEL_PAIRS: usize = 200;

pub(crate) fn worker_count(requested: usize, len: usize) -> usize {
    if requested == 0 {
        if len < MIN_AUTO_PARALLEL_PAIRS {
            return 1;
        }
        return thread::available_parallelism()
            .map_or(1, |cpus| cpus.get().saturating_sub(1).max(1))
            .min(len);
    }
    requested.min(len)
}

struct StopWorkers<'a>(&'a AtomicBool);

impl Drop for StopWorkers<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Keep one outstanding item per worker. The caller consumes results, runs hooks, and
/// admits new work in input order while workers can finish sampling in any order.
pub(crate) fn for_each_ordered<T: Copy + Send, R: Send, E>(
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
}
