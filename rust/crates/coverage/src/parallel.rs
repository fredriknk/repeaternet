use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Mutex,
    },
    thread,
    time::Duration,
};

use crate::CoverageError;

pub(crate) fn worker_count(requested: usize, items: usize, links: usize) -> usize {
    const MIN_AUTO_PARALLEL_LINKS: usize = 512;

    let limit = match requested {
        0 if links < MIN_AUTO_PARALLEL_LINKS => 1,
        0 => thread::available_parallelism().map_or(1, |cpus| (cpus.get() - 1).max(1)),
        count => count,
    };
    limit.min(items)
}

fn accept_result<R>(
    result: Result<R, CoverageError>,
    accept: &mut impl FnMut(R) -> Result<(), CoverageError>,
) -> Result<bool, CoverageError> {
    match result {
        Err(CoverageError::Cancelled) => Ok(false),
        result => {
            accept(result?)?;
            Ok(true)
        }
    }
}

pub(crate) fn for_each_serial<I, R>(
    items: impl Iterator<Item = I>,
    cancelled: &dyn Fn() -> bool,
    evaluate: impl Fn(I, &dyn Fn() -> bool) -> Result<R, CoverageError>,
    mut accept: impl FnMut(R) -> Result<(), CoverageError>,
) -> Result<bool, CoverageError> {
    for item in items {
        if cancelled() || !accept_result(evaluate(item, cancelled), &mut accept)? {
            return Ok(false);
        }
    }
    Ok(true)
}

struct StopWorkers<'a>(&'a AtomicBool);

impl Drop for StopWorkers<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct StopOnPanic<'a>(&'a AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

/// Accept results in input order while allowing workers to finish in any order.
/// Only acceptance opens slots in the window, bounding tasks and unaccepted results
/// even if the first item is slow. Cancellation stops acceptance on the calling
/// thread and signals workers between link evaluations. Returns false if interrupted.
pub(crate) fn for_each_ordered<I: Send, R: Send>(
    items: impl ExactSizeIterator<Item = I>,
    workers: usize,
    cancelled: &dyn Fn() -> bool,
    evaluate: impl Fn(I, &dyn Fn() -> bool) -> Result<R, CoverageError> + Sync,
    mut accept: impl FnMut(R) -> Result<(), CoverageError>,
) -> Result<bool, CoverageError> {
    if workers <= 1 {
        return for_each_serial(items, cancelled, evaluate, accept);
    }
    if cancelled() {
        return Ok(false);
    }

    let count = items.len();
    let window = workers.saturating_mul(2).min(count);
    let (tasks, queued) = mpsc::sync_channel(window);
    let queued = Mutex::new(queued);
    let (results, finished) = mpsc::channel();
    let stop = AtomicBool::new(false);
    let worker_cancelled = || stop.load(Ordering::Relaxed);
    let evaluate = &evaluate;
    thread::scope(|scope| {
        let tasks = tasks;
        let _stop = StopWorkers(&stop);
        let mut spawned = 0usize;
        for _ in 0..workers {
            if cancelled() {
                return Ok(false);
            }
            let queued = &queued;
            let results = results.clone();
            let stop = &stop;
            if thread::Builder::new()
                .spawn_scoped(scope, move || {
                    let _stop = StopOnPanic(stop);
                    while !worker_cancelled() {
                        let task = queued.lock().unwrap().recv();
                        let Ok((index, item)) = task else { break };
                        if worker_cancelled()
                            || results
                                .send((index, evaluate(item, &worker_cancelled)))
                                .is_err()
                        {
                            break;
                        }
                    }
                })
                .is_err()
            {
                break;
            }
            spawned += 1;
        }
        drop(results);
        if spawned == 0 {
            return for_each_serial(items, cancelled, evaluate, accept);
        }

        let window = window.min(spawned.saturating_mul(2));
        let mut remaining = items.enumerate();
        for task in remaining.by_ref().take(window) {
            tasks.send(task).expect("workers can receive initial tasks");
        }
        let mut ready: Vec<_> = std::iter::repeat_with(|| None).take(window).collect();
        for next in 0..count {
            let slot = next % window;
            while ready[slot].is_none() {
                if worker_cancelled() || cancelled() {
                    return Ok(false);
                }
                match finished.recv_timeout(Duration::from_millis(10)) {
                    Ok((index, result)) => ready[index % window] = Some(result),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        // The scope propagates any worker panic.
                        return Ok(false);
                    }
                }
            }
            if worker_cancelled()
                || cancelled()
                || !accept_result(ready[slot].take().unwrap(), &mut accept)?
            {
                return Ok(false);
            }
            if let Some(task) = remaining.next() {
                tasks.send(task).expect("workers can receive the next task");
            }
        }
        Ok(true)
    })
}
