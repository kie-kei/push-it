use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cron::Schedule;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::db::{App, Revision};
use crate::manifest::Manifest;
use crate::server::{resolve_call_env_caps, AppState};

/// Five-field UTC cron, including Unix weekday numbering and DOM/DOW union.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CronSchedule {
    primary: Schedule,
    weekday: Option<Schedule>,
}

impl CronSchedule {
    pub(crate) fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<_> = expression.split_whitespace().collect();
        anyhow::ensure!(
            fields.len() == 5,
            "expected five fields: minute hour day-of-month month day-of-week"
        );
        anyhow::ensure!(!expression.contains('?'), "use '*' for unrestricted fields");
        let weekday = weekdays(fields[4])?;
        // The crate uses Sunday=1 and intersects DOM/DOW; Unix cron uses
        // Sunday=0 (or 7) and unions the two when both are restricted.
        let (primary, extra) = if !fields[2].starts_with('*') && !fields[4].starts_with('*') {
            (
                format!(
                    "0 {} {} {} {} *",
                    fields[0], fields[1], fields[2], fields[3]
                ),
                Some(format!(
                    "0 {} {} * {} {}",
                    fields[0], fields[1], fields[3], weekday
                )),
            )
        } else {
            (
                format!(
                    "0 {} {} {} {} {}",
                    fields[0], fields[1], fields[2], fields[3], weekday
                ),
                None,
            )
        };
        Ok(Self {
            primary: Schedule::from_str(&primary)?,
            weekday: extra.map(|s| Schedule::from_str(&s)).transpose()?,
        })
    }

    fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let first = self.primary.after(&now).next();
        let second = self.weekday.as_ref().and_then(|s| s.after(&now).next());
        first.into_iter().chain(second).min()
    }
}

fn weekdays(field: &str) -> Result<String> {
    fn ordinal(s: &str) -> Result<u32> {
        let value = match s.to_ascii_uppercase().as_str() {
            "SUN" => 0,
            "MON" => 1,
            "TUE" => 2,
            "WED" => 3,
            "THU" => 4,
            "FRI" => 5,
            "SAT" => 6,
            _ => s.parse().context("invalid day-of-week")?,
        };
        anyhow::ensure!(value <= 7, "day-of-week must be 0-7 or SUN-SAT");
        Ok(value)
    }
    let mut days = std::collections::BTreeSet::new();
    for item in field.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => (base, step.parse::<u32>().context("invalid weekday step")?),
            None => (item, 1),
        };
        anyhow::ensure!(step > 0, "weekday step must be positive");
        let (start, end) = if base == "*" {
            (0, 6)
        } else if let Some((start, end)) = base.split_once('-') {
            (ordinal(start)?, ordinal(end)?)
        } else {
            let start = ordinal(base)?;
            (start, if item.contains('/') { 7 } else { start })
        };
        anyhow::ensure!(start <= end, "reversed weekday range");
        for day in (start..=end).step_by(step as usize) {
            days.insert((day % 7 + 1).to_string());
        }
    }
    Ok(days.into_iter().collect::<Vec<_>>().join(","))
}

type JobKey = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Job {
    app: String,
    name: String,
    path: String,
    schedule: CronSchedule,
}

impl Job {
    fn key(&self) -> JobKey {
        (self.app.clone(), self.name.clone())
    }
}

struct Pending {
    job: Job,
    next_due: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Scheduler {
    jobs: HashMap<JobKey, Pending>,
    in_flight: HashSet<JobKey>,
}

impl Scheduler {
    /// The caller supplies the clock and execution closure; no wall clock or spawning here.
    fn tick(&mut self, now: DateTime<Utc>, jobs: Vec<Job>, mut run: impl FnMut(&Job)) {
        let mut retained = HashSet::new();
        for job in jobs {
            let key = job.key();
            retained.insert(key.clone());
            let pending = self.jobs.entry(key.clone()).or_insert_with(|| Pending {
                next_due: job.schedule.next_after(now),
                job: job.clone(),
            });
            if pending.job != job {
                pending.next_due = job.schedule.next_after(now);
                pending.job = job;
            }
            if pending.next_due.is_some_and(|due| now >= due) {
                // Missed and overlapping occurrences are skipped, never queued.
                pending.next_due = pending.job.schedule.next_after(now);
                if self.in_flight.insert(key) {
                    run(&pending.job);
                }
            }
        }
        self.jobs.retain(|key, _| retained.contains(key));
    }
}

fn tick_seconds(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .filter(|v| {
            tokio::time::Instant::now()
                .checked_add(Duration::from_secs(*v))
                .is_some()
        })
        .unwrap_or(30)
}

fn scan(s: &AppState) -> Result<(Vec<Job>, HashMap<String, (App, Revision)>)> {
    let mut jobs = Vec::new();
    let mut snapshots = HashMap::new();
    for (app, revision) in s.db.active_apps()? {
        let manifest = match Manifest::parse(&app.manifest) {
            Ok(m) => m,
            Err(error) => {
                tracing::warn!(app = %app.name, %error, "cron manifest scan failed");
                continue;
            }
        };
        for entry in manifest.cron {
            jobs.push(Job {
                app: app.name.clone(),
                name: entry.name,
                path: entry.path,
                schedule: CronSchedule::parse(&entry.schedule)?,
            });
        }
        snapshots.insert(app.name.clone(), (app, revision));
    }
    Ok((jobs, snapshots))
}

pub async fn run(s: Arc<AppState>) {
    let secs = tick_seconds(std::env::var("PUSHIT_CRON_TICK_SECS").ok().as_deref());
    let mut timer = tokio::time::interval(Duration::from_secs(secs));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut scheduler = Scheduler::default();
    let mut runs = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = timer.tick() => {
                match scan(&s) {
                    Ok((jobs, snapshots)) => scheduler.tick(Utc::now(), jobs, |job| {
                        let (app, revision) = snapshots[&job.app].clone();
                        let state = s.clone();
                        let job = job.clone();
                        runs.spawn(async move {
                            let key = job.key();
                            // Join the invocation separately so even a panic clears its in-flight key.
                            let result = tokio::spawn(async move {
                                invoke(&state, &app, &revision, &job).await
                            }).await;
                            (key, result)
                        });
                    }),
                    Err(error) => tracing::warn!(%error, "cron scan failed"),
                }
            }
            Some(result) = runs.join_next(), if !runs.is_empty() => {
                match result {
                    Ok((key, result)) => {
                        scheduler.in_flight.remove(&key);
                        match result {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => tracing::warn!(app = %key.0, job = %key.1, %error, "cron run failed"),
                            Err(error) => tracing::warn!(app = %key.0, job = %key.1, %error, "cron task failed"),
                        }
                    }
                    Err(error) => tracing::warn!(%error, "cron supervisor task failed"),
                }
            }
        }
    }
}

async fn invoke(s: &AppState, app: &App, revision: &Revision, job: &Job) -> Result<()> {
    let (completion, done) = tokio::sync::oneshot::channel::<()>();
    let result = async {
        let Some((revision, pin)) = s.db.pin_revision(&app.name, &revision.id)? else {
            bail!("scheduled revision no longer exists");
        };
        let manifest = Manifest::parse(&app.manifest)?;
        let (mut env, mut caps) = resolve_call_env_caps(s, app, &manifest)?;
        env.retain(|(name, _)| name != "PUSHIT_CRON");
        env.push(("PUSHIT_CRON".into(), job.name.clone()));
        let _pin = Arc::new(pin);
        caps.artifact_pin = Some(_pin.clone());
        caps.completion = Some(completion);
        let request = hyper::Request::builder()
            .method("GET")
            .uri(&job.path)
            .header("host", &app.domain)
            .header("x-user-id", "cron")
            .header("x-user-name", "cron")
            .header("x-user-email", "cron@pushit.local")
            .header("x-pushit-cron", &job.name)
            .body(Full::new(Bytes::new()))?;
        let mut res = s
            .runtime
            .call(
                std::path::Path::new(&revision.artifact_dir),
                &app.name,
                &s.cfg.data_dir,
                env,
                caps,
                request,
            )
            .await?;
        s.logs.append(&app.name, &res.stdout)?;
        s.logs.append(&app.name, &res.stderr)?;
        let status = res
            .response
            .as_ref()
            .map(|r| r.status().as_u16())
            .unwrap_or(204);
        if let Some(response) = res.response.take() {
            let mut body = response.into_body();
            tokio::time::timeout(s.runtime.timeout, async {
                while let Some(frame) = body.frame().await {
                    frame.map_err(|e| anyhow::anyhow!("cron response body: {e:?}"))?;
                }
                anyhow::Ok(())
            })
            .await
            .context("cron response body timed out")??;
        }
        Ok::<_, anyhow::Error>(status)
    }
    .await;
    let summary = match &result {
        Ok(status) => status.to_string(),
        Err(error) => format!("error: {error:#}").replace(['\r', '\n'], " "),
    };
    let logged = s.logs.append(
        &app.name,
        format!("cron '{}' GET {} -> {}\n", job.name, job.path, summary).as_bytes(),
    );
    // The runtime may return an error before its guest task has stopped.
    // Keep the job in flight until that task drops its completion sender.
    let _ = done.await;
    logged?;
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn clock(minute: u32, second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 26, 9, minute, second)
            .unwrap()
    }

    fn job() -> Job {
        Job {
            app: "a".into(),
            name: "tick".into(),
            path: "/_fn/log".into(),
            schedule: CronSchedule::parse("* * * * *").unwrap(),
        }
    }

    #[test]
    fn initializes_strictly_after_now_and_fires_when_due() {
        let mut scheduler = Scheduler::default();
        let mut runs = Vec::new();
        scheduler.tick(clock(0, 0), vec![job()], |j| runs.push(j.key()));
        assert_eq!(scheduler.jobs[&job().key()].next_due, Some(clock(1, 0)));
        scheduler.tick(clock(0, 59), vec![job()], |j| runs.push(j.key()));
        assert!(runs.is_empty());
        scheduler.tick(clock(1, 0), vec![job()], |j| runs.push(j.key()));
        assert_eq!(runs, vec![job().key()]);
        assert_eq!(scheduler.jobs[&job().key()].next_due, Some(clock(2, 0)));
    }

    #[test]
    fn skips_missed_occurrences_and_never_overlaps() {
        let mut scheduler = Scheduler::default();
        let mut runs = Vec::new();
        scheduler.tick(clock(0, 30), vec![job()], |_| {});
        scheduler.tick(clock(4, 30), vec![job()], |j| runs.push(j.key()));
        assert_eq!(runs.len(), 1);
        assert_eq!(scheduler.jobs[&job().key()].next_due, Some(clock(5, 0)));
        scheduler.tick(clock(6, 0), vec![job()], |j| runs.push(j.key()));
        assert_eq!(runs.len(), 1);
        scheduler.in_flight.remove(&job().key());
        scheduler.tick(clock(6, 1), vec![job()], |j| runs.push(j.key()));
        assert_eq!(runs.len(), 1);
        scheduler.tick(clock(7, 0), vec![job()], |j| runs.push(j.key()));
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn drops_removed_jobs_and_reinitializes_changed_jobs() {
        let mut scheduler = Scheduler::default();
        scheduler.tick(clock(0, 0), vec![job()], |_| {});
        let mut changed = job();
        changed.path = "/_cron/new".into();
        scheduler.tick(clock(1, 0), vec![changed], |_| {
            panic!("changed job caught up")
        });
        assert_eq!(scheduler.jobs[&job().key()].next_due, Some(clock(2, 0)));
        scheduler.tick(clock(2, 0), vec![], |_| panic!("removed job fired"));
        assert!(scheduler.jobs.is_empty());
        scheduler.tick(clock(3, 0), vec![job()], |_| panic!("new job caught up"));
        assert_eq!(scheduler.jobs[&job().key()].next_due, Some(clock(4, 0)));
    }

    #[test]
    fn in_flight_survives_removal_readdition_and_replacement() {
        let mut scheduler = Scheduler::default();
        let mut runs = Vec::new();
        scheduler.tick(clock(0, 0), vec![job()], |_| {});
        scheduler.tick(clock(1, 0), vec![job()], |j| runs.push(j.key()));
        scheduler.tick(clock(2, 0), vec![], |_| panic!("removed job fired"));
        scheduler.tick(clock(3, 0), vec![job()], |_| panic!("new job caught up"));
        scheduler.tick(clock(4, 0), vec![job()], |_| {
            panic!("re-added job overlapped")
        });
        let mut changed = job();
        changed.path = "/_cron/replacement".into();
        scheduler.tick(clock(4, 1), vec![changed.clone()], |_| {});
        scheduler.tick(clock(5, 0), vec![changed.clone()], |_| {
            panic!("replacement overlapped")
        });
        scheduler.in_flight.remove(&job().key());
        scheduler.tick(clock(6, 0), vec![changed], |j| runs.push(j.key()));
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn unix_weekdays_and_day_of_month_union() {
        for expression in ["0 9 * * 0", "0 9 * * 7", "0 9 * * SUN"] {
            let next = CronSchedule::parse(expression)
                .unwrap()
                .next_after(clock(0, 0))
                .unwrap();
            assert_eq!(next, Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap());
        }
        let next = CronSchedule::parse("0 9 1 * MON-FRI")
            .unwrap()
            .next_after(clock(0, 0))
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 10, 27, 9, 0, 0).unwrap());
        assert_eq!(
            CronSchedule::parse("0 9 * * 1-5/2")
                .unwrap()
                .next_after(clock(0, 0))
                .unwrap(),
            Utc.with_ymd_and_hms(2026, 10, 28, 9, 0, 0).unwrap()
        );
        for expression in [
            "* * * * 8",
            "* * * * */0",
            "* * * * 5-1",
            "@daily",
            "* * * * * *",
        ] {
            assert!(CronSchedule::parse(expression).is_err(), "{expression}");
        }
    }

    #[test]
    fn tick_configuration_defaults_safely() {
        for value in [
            None,
            Some("bad"),
            Some("0"),
            Some("-1"),
            Some("18446744073709551615"),
        ] {
            assert_eq!(tick_seconds(value), 30);
        }
        assert_eq!(tick_seconds(Some("1")), 1);
    }
}
