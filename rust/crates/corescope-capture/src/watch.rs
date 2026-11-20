use std::{thread, time::Duration};

use anyhow::{Result, ensure};
use chrono::{DateTime, Local, Utc};
use repeaternet_providers::{ProviderError, corescope::capture::CaptureClient};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    Cli, Mode, capture,
    db::{self, Run},
};

const PACKET_MINIMUM: i64 = 5 * 60;
const SNAPSHOT_MINIMUM: i64 = 60 * 60;
const MAX_BACKOFF: u64 = 60 * 60;
const WATCH_PACKET_PAGES: usize = 8;
const SNAPSHOT_PAGES: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Task {
    Topology,
    Packets,
}

struct Schedule {
    packet_at: i64,
    topology_at: i64,
    pause_until: i64,
    failures: u32,
    backoff_reason: Option<String>,
}

fn interval(seconds: i64, minimum: i64, jitter: f64) -> i64 {
    ((seconds as f64 * (1.0 + jitter)).round() as i64).max(minimum)
}

fn next_interval(minutes: u64, minimum: i64) -> i64 {
    let jitter = getrandom::u32().map_or(0.0, |value| {
        f64::from(value) / f64::from(u32::MAX) * 0.2 - 0.1
    });
    interval(minutes as i64 * 60, minimum, jitter)
}

fn backoff(failures: u32, retry_after: Duration) -> i64 {
    let seconds = (60 << failures.saturating_sub(1).min(6)).min(MAX_BACKOFF);
    Duration::from_secs(seconds)
        .max(retry_after)
        .as_secs_f64()
        .ceil() as i64
}

impl Schedule {
    fn next(&self) -> (Task, i64) {
        let (task, due) = if self.topology_at <= self.packet_at {
            (Task::Topology, self.topology_at)
        } else {
            (Task::Packets, self.packet_at)
        };
        (task, due.max(self.pause_until))
    }

    fn next_poll(&self) -> i64 {
        self.packet_at.max(self.pause_until)
    }

    fn store(&self, db: &Connection, endpoint: &str, last_poll: Option<i64>) -> Result<()> {
        db.execute(
            "INSERT INTO watch_state VALUES (?1, ?2, ?3, NULLIF(?4, 0), ?5, ?6)
             ON CONFLICT(endpoint) DO UPDATE SET
                 last_poll = COALESCE(excluded.last_poll, watch_state.last_poll),
                 next_poll = excluded.next_poll, backoff_until = excluded.backoff_until,
                 backoff_reason = excluded.backoff_reason, failures = excluded.failures",
            params![
                endpoint,
                last_poll,
                self.next_poll().max(Utc::now().timestamp()),
                self.pause_until,
                self.backoff_reason,
                self.failures
            ],
        )?;
        Ok(())
    }
}

pub fn run(
    db: &mut Connection,
    client: &mut CaptureClient,
    cli: &Cli,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    client.defer_retries();
    let endpoint = client.base_url().to_owned();
    let now = Utc::now().timestamp();
    let (pause_until, failures, backoff_reason) = db.query_row(
        "SELECT COALESCE(backoff_until, 0), failures, backoff_reason FROM watch_state WHERE endpoint = ?",
        [&endpoint],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?.unwrap_or_default();
    let mut schedule = Schedule {
        packet_at: now,
        topology_at: db::latest_topology(db, &endpoint)?.map_or(now, |last| {
            (last + next_interval(cli.snapshot_minutes, SNAPSHOT_MINIMUM)).max(now)
        }),
        pause_until,
        failures,
        backoff_reason,
    };
    schedule.store(db, &endpoint, None)?;
    println!("Watching CoreScope · Ctrl+C to stop");
    while !stopped() {
        let (task, due) = schedule.next();
        if due > Utc::now().timestamp() {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        let mut last_poll = None;
        let request_start = client.requests();
        let (mode, label) = match task {
            Task::Topology => ("watch-topology", "Topology"),
            Task::Packets => ("watch-packets", "Packets"),
        };
        let mut run = Run::start(db, &endpoint, mode)?;
        let result = match task {
            Task::Topology => {
                capture::topology(db, client, pages(cli, SNAPSHOT_PAGES), &mut run, stopped)
            }
            Task::Packets => match capture::packets(
                db,
                client,
                pages(cli, WATCH_PACKET_PAGES),
                &mut run,
                stopped,
            ) {
                Ok(()) if !stopped() => {
                    last_poll = Some(Utc::now().timestamp());
                    capture::neighbors(
                        db,
                        client,
                        cli.bbox,
                        cli.neighbor_batch as usize,
                        &mut run,
                        stopped,
                    )
                }
                result => result,
            },
        };
        record_error(&mut run, result);
        let requests = client.requests() - request_start;
        let status = run.finish(db, requests, stopped())?;
        let now = Utc::now().timestamp();
        match task {
            Task::Packets => {
                schedule.packet_at = now + next_interval(cli.packet_minutes, PACKET_MINIMUM)
            }
            Task::Topology if run.errors.is_empty() => {
                schedule.topology_at = now + next_interval(cli.snapshot_minutes, SNAPSHOT_MINIMUM);
            }
            Task::Topology => schedule.topology_at = now + SNAPSHOT_MINIMUM,
        }
        if let Some(error) = run.errors.first() {
            schedule.failures = schedule.failures.saturating_add(1);
            schedule.pause_until =
                now.saturating_add(backoff(schedule.failures, client.retry_after()));
            schedule.backoff_reason = Some(error.clone());
        } else if !stopped() {
            schedule.failures = 0;
            schedule.pause_until = 0;
            schedule.backoff_reason = None;
        }
        schedule.store(db, &endpoint, last_poll)?;
        let next = if stopped() {
            "stopped".to_owned()
        } else if schedule.next_poll() <= now {
            "now".to_owned()
        } else {
            local_time(schedule.next_poll())
        };
        let pause = schedule
            .backoff_reason
            .as_ref()
            .map_or_else(String::new, |reason| {
                format!(
                    " · backoff until {} ({reason})",
                    local_time(schedule.pause_until)
                )
            });
        println!(
            "{} · {label} {status} · +{} receptions · {} pairs · {requests}/{} requests (cycle/24 h) · next poll {next}{pause}",
            Local::now().format("%F %T"),
            run.observations,
            db::pair_count(db, &endpoint)?,
            db::requests_today(db, &endpoint, Utc::now().timestamp_millis())?,
        );
    }
    db.execute(
        "UPDATE watch_state SET next_poll = NULL WHERE endpoint = ?",
        [&endpoint],
    )?;
    println!("Capture stopped. Database saved.");
    Ok(())
}

fn local_time(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || "scheduled".to_owned(),
        |time| time.with_timezone(&Local).format("%F %T").to_string(),
    )
}

fn record_error(run: &mut Run, result: Result<()>) {
    if let Err(error) = result
        && !matches!(error.downcast_ref(), Some(ProviderError::Cancelled))
    {
        run.errors.push(format!("{error:#}"));
    }
}

fn pages(cli: &Cli, default: usize) -> usize {
    cli.max_pages.map_or(default, std::num::NonZeroUsize::get)
}

pub fn once(
    db: &mut Connection,
    client: &mut CaptureClient,
    cli: &Cli,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let endpoint = client.base_url().to_owned();
    let max_pages = pages(cli, SNAPSHOT_PAGES);
    if cli.mode != Mode::Packets && !stopped() {
        let mut run = Run::start(db, &endpoint, "snapshot")?;
        let start = client.requests();
        let result = capture::topology(db, client, max_pages, &mut run, stopped);
        record_error(&mut run, result);
        finish_once(db, client, run, start, stopped)?;
    }
    if cli.mode != Mode::Snapshot && !stopped() {
        let mut run = Run::start(db, &endpoint, "packets")?;
        let start = client.requests();
        let result = capture::packets(db, client, max_pages, &mut run, stopped);
        record_error(&mut run, result);
        finish_once(db, client, run, start, stopped)?;
    }
    if cli.mode != Mode::Packets && !stopped() && cli.neighbor_batch > 0 {
        let mut run = Run::start(db, &endpoint, "neighbors")?;
        let start = client.requests();
        let result = capture::neighbors(
            db,
            client,
            cli.bbox,
            cli.neighbor_batch as usize,
            &mut run,
            stopped,
        );
        record_error(&mut run, result);
        finish_once(db, client, run, start, stopped)?;
    }
    Ok(())
}

fn finish_once(
    db: &Connection,
    client: &CaptureClient,
    run: Run,
    start: usize,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let requests = client.requests() - start;
    let status = run.finish(db, requests, stopped())?;
    println!(
        "Capture {} {status} · {} receptions added · {} nodes · {} observers · {} neighbor responses · {requests} requests",
        run.id, run.observations, run.nodes, run.observers, run.neighbors,
    );
    ensure!(run.errors.is_empty(), "{}", run.errors.join("\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn watch_defaults_and_cli_floors() {
        let cli = Cli::try_parse_from(["capture", "watch"]).unwrap();
        assert_eq!(
            (cli.packet_minutes, cli.snapshot_minutes, cli.neighbor_batch),
            (15, 360, 20)
        );
        assert_eq!(cli.rate, Duration::from_secs(2));
        for (flag, value) in [
            ("--packet-minutes", "4"),
            ("--snapshot-minutes", "59"),
            ("--rate", "1.001"),
            ("--rate", "NaN"),
            ("--neighbor-batch", "21"),
        ] {
            assert!(Cli::try_parse_from(["capture", "watch", flag, value]).is_err());
        }
        assert_eq!(crate::parse_rate("1").unwrap(), Duration::from_secs(1));
        assert_eq!(interval(900, PACKET_MINIMUM, -0.1), 810);
        assert_eq!(interval(900, PACKET_MINIMUM, 0.1), 990);
        assert_eq!(interval(300, PACKET_MINIMUM, -0.1), 300);
        assert_eq!(interval(3600, SNAPSHOT_MINIMUM, -0.1), 3600);
    }

    #[test]
    fn independent_due_times_and_server_wide_backoff() {
        let mut schedule = Schedule {
            packet_at: 100,
            topology_at: 200,
            pause_until: 0,
            failures: 0,
            backoff_reason: None,
        };
        assert!(matches!(schedule.next(), (Task::Packets, 100)));
        schedule.packet_at = 250;
        assert!(matches!(schedule.next(), (Task::Topology, 200)));
        schedule.pause_until = 300;
        assert!(matches!(schedule.next(), (Task::Topology, 300)));
        assert_eq!(schedule.next_poll(), 300);
        assert_eq!(backoff(1, Duration::ZERO), 60);
        assert_eq!(backoff(2, Duration::ZERO), 120);
        assert_eq!(backoff(20, Duration::ZERO), 3600);
        assert_eq!(backoff(20, Duration::from_secs(7200)), 7200);
    }
}
