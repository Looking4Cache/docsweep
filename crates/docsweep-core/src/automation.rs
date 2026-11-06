//! Runs that nobody started by hand: `portal.schedule` and `mail.poll_minutes`.
//!
//! A small loop in the app calls [`run_tick`] every few seconds. Each tick works
//! out from the clock and the index what is due. Nothing is remembered as done
//! before it actually starts, so what cannot start because another run is active
//! simply stays due and happens at the next tick.
//!
//! The time of the last scheduled run lives in the index, and only one run follows
//! however many dates were missed. A run counts as soon as it starts, so a failed
//! one waits for the next date instead of repeating a login every few seconds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, TimeDelta, TimeZone, Utc};

use crate::config::Config;
use crate::index::{Index, IndexError};
use crate::mail::execute::{ScanError, ScanReport, run_mail_scan};
use crate::portal::driver::PortalFetcher;
use crate::schedule::Schedule;
use crate::secrets::SecretStore;

/// Only one run at a time, whoever started it. Scans, previews and fetches by
/// hand take the same permit as automatic runs.
#[derive(Debug, Default)]
pub struct RunGate(AtomicBool);

impl RunGate {
    /// The permit to run, or `None` while another run is active.
    pub fn try_start(&self) -> Option<RunPermit<'_>> {
        // A permit opens the gate when dropped, so none may exist on the way to `None`.
        (!self.0.swap(true, Ordering::SeqCst)).then(|| RunPermit(&self.0))
    }
}

/// Opens the gate again when dropped, even if the run ends early.
#[derive(Debug)]
pub struct RunPermit<'a>(&'a AtomicBool);

impl Drop for RunPermit<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What is due right now.
#[derive(Debug, PartialEq, Eq)]
pub struct DueWork {
    pub poll: bool,
    /// The ids of the companies whose schedule has come up.
    pub companies: Vec<String>,
}

/// Works out what is due. A schedule seen for the first time starts counting now,
/// so setting one up does not run it at once.
pub fn due_work<Tz: TimeZone>(
    config: &Config,
    index: &Index,
    last_poll: Option<DateTime<Utc>>,
    now: &DateTime<Tz>,
) -> Result<DueWork, IndexError> {
    let poll = config.mail.poll_minutes.is_some_and(|minutes| {
        last_poll.is_none_or(|last| now.to_utc() - last >= TimeDelta::minutes(i64::from(minutes)))
    });
    let mut companies = Vec::new();
    for company in &config.companies {
        let schedule = company.portal.as_ref().and_then(|p| p.schedule.as_deref());
        let Some(Ok(schedule)) = schedule.map(Schedule::parse) else {
            continue;
        };
        match index.last_scheduled_run(&company.id)? {
            None => index.note_scheduled_run(&company.id, now.to_utc())?,
            Some(last) => {
                let next = schedule.next_after(&last.with_timezone(&now.timezone()));
                if next.is_some_and(|next| next <= *now) {
                    companies.push(company.id.clone());
                }
            }
        }
    }
    Ok(DueWork { poll, companies })
}

/// What came of a tick.
#[derive(Debug)]
pub enum Tick {
    /// Automation is switched off.
    Off,
    /// Another run is active; what is due stays due.
    Busy,
    /// Nothing is due.
    Idle,
    Ran(ScanReport),
}

/// One turn of the loop. `on_start` runs once if there is something to do.
#[allow(
    clippy::too_many_arguments,
    reason = "everything a tick depends on, passed in so tests can control it"
)]
pub async fn run_tick<Tz: TimeZone, F: PortalFetcher>(
    config: &Config,
    secrets: &dyn SecretStore,
    index_path: &Path,
    gate: &RunGate,
    last_poll: &mut Option<DateTime<Utc>>,
    now: DateTime<Tz>,
    fetcher: &mut F,
    on_start: impl FnOnce(),
) -> Result<Tick, ScanError> {
    if !config.automation.enabled {
        return Ok(Tick::Off);
    }
    let Some(_permit) = gate.try_start() else {
        return Ok(Tick::Busy);
    };
    let index = Index::open(index_path)?;
    let due = due_work(config, &index, *last_poll, &now)?;
    if !due.poll && due.companies.is_empty() {
        return Ok(Tick::Idle);
    }
    on_start();

    let mut report = ScanReport::default();
    if due.poll {
        *last_poll = Some(now.to_utc());
        report = run_mail_scan(config, secrets, index_path, now.date_naive(), fetcher).await?;
    }
    fetch_scheduled(
        config,
        &index,
        &due.companies,
        now.to_utc(),
        &mut report,
        fetcher,
    )
    .await?;
    Ok(Tick::Ran(report))
}

/// Starts the scheduled recipes, one company after the other. A link mail may have
/// started a company's recipe a moment ago, and that counts as this run.
async fn fetch_scheduled<F: PortalFetcher>(
    config: &Config,
    index: &Index,
    due: &[String],
    now: DateTime<Utc>,
    report: &mut ScanReport,
    fetcher: &mut F,
) -> Result<(), IndexError> {
    for company in config.companies.iter().filter(|c| due.contains(&c.id)) {
        index.note_scheduled_run(&company.id, now)?;
        if !report.fetches.iter().any(|f| f.company_id == company.id) {
            report.fetches.push(fetcher.fetch(company).await);
        }
    }
    Ok(())
}

/// Whether an automatic run belongs in the history. Polls that found nothing to
/// do would only bury the runs that matter.
pub fn worth_recording(report: &ScanReport) -> bool {
    let summary = report.summary();
    !report.fetches.is_empty()
        || summary.saved + summary.for_review + summary.disposed + summary.failed > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Company;
    use crate::portal::engine::FetchReport;
    use crate::secrets::MemoryStore;
    use chrono::FixedOffset;
    use std::sync::Arc;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
    }

    fn config(extra: &str) -> Config {
        Config::from_yaml(&format!(
            r#"{extra}
mail:
  accounts:
    - {{ id: main, imap: "127.0.0.1:1", user: u, password: "keyring:absent", watch: INBOX }}
  disposal: {{ mode: all_delete }}
companies:
  - id: a
    name: A
    portal: {{ credentials: "keyring:a", recipe: a.yaml, schedule: "0 9 10 * *" }}
    output: {{ dir: /x, filename: "{{invoice_no}}.pdf" }}
  - id: b
    name: B
    portal: {{ credentials: "keyring:b", recipe: b.yaml, schedule: "0 9 15 * *" }}
    output: {{ dir: /x, filename: "{{invoice_no}}.pdf" }}
  - id: c
    name: C
    portal: {{ credentials: "keyring:c", recipe: c.yaml }}
    output: {{ dir: /x, filename: "{{invoice_no}}.pdf" }}
"#
        ))
        .unwrap()
    }

    const ON: &str = "automation: { enabled: true }";

    fn with_polling(minutes: u32) -> Config {
        let mut config = config(ON);
        config.mail.poll_minutes = Some(minutes);
        config
    }

    #[derive(Default)]
    struct FakeFetcher {
        started: Vec<String>,
        failing: Option<&'static str>,
    }

    impl PortalFetcher for FakeFetcher {
        async fn fetch(&mut self, company: &Company) -> FetchReport {
            self.started.push(company.id.clone());
            if self.failing == Some(company.id.as_str()) {
                return FetchReport::failed(company, "nobody answered in time".into());
            }
            FetchReport {
                company_id: company.id.clone(),
                company_name: company.name.clone(),
                entries: Vec::new(),
                error: None,
            }
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// A temporary index and what ticks share.
    struct World {
        dir: tempfile::TempDir,
        gate: Arc<RunGate>,
        last_poll: Option<DateTime<Utc>>,
        fetcher: FakeFetcher,
    }

    impl World {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
                gate: Arc::default(),
                last_poll: None,
                fetcher: FakeFetcher::default(),
            }
        }

        fn index_path(&self) -> std::path::PathBuf {
            self.dir.path().join("docsweep.db")
        }

        fn index(&self) -> Index {
            Index::open(&self.index_path()).unwrap()
        }

        fn tick(&mut self, config: &Config, now: DateTime<FixedOffset>) -> Tick {
            block_on(run_tick(
                config,
                &MemoryStore::default(),
                &self.index_path(),
                &self.gate,
                &mut self.last_poll,
                now,
                &mut self.fetcher,
                || {},
            ))
            .unwrap()
        }
    }

    #[test]
    fn nothing_runs_unless_automation_is_switched_on() {
        let mut world = World::new();
        let config = config("");
        world
            .index()
            .note_scheduled_run("a", at(2026, 1, 1, 0, 0).to_utc())
            .unwrap();

        assert!(matches!(
            world.tick(&config, at(2026, 11, 10, 9, 0)),
            Tick::Off
        ));

        assert!(world.fetcher.started.is_empty());
        assert_eq!(
            world.index().last_scheduled_run("a").unwrap(),
            Some(at(2026, 1, 1, 0, 0).to_utc())
        );
    }

    #[test]
    fn switched_on_without_a_poll_interval_it_does_not_touch_the_mailboxes() {
        let config = config(ON);
        let index = Index::open_in_memory().unwrap();
        let due = due_work(&config, &index, None, &at(2026, 11, 6, 12, 0)).unwrap();
        assert!(!due.poll);
    }

    #[test]
    fn a_new_schedule_starts_counting_now_instead_of_running_at_once() {
        let mut world = World::new();
        let config = config(ON);

        assert!(matches!(
            world.tick(&config, at(2026, 11, 6, 12, 0)),
            Tick::Idle
        ));
        assert!(world.fetcher.started.is_empty());

        assert!(matches!(
            world.tick(&config, at(2026, 11, 10, 8, 59)),
            Tick::Idle
        ));
        assert!(matches!(
            world.tick(&config, at(2026, 11, 10, 9, 0)),
            Tick::Ran(_)
        ));
        assert_eq!(world.fetcher.started, ["a"]);

        assert!(matches!(
            world.tick(&config, at(2026, 11, 10, 9, 1)),
            Tick::Idle
        ));
        assert_eq!(world.fetcher.started, ["a"], "not twice for one date");
    }

    #[test]
    fn the_schedule_counts_in_the_given_local_time() {
        let config = config(ON);
        let index = Index::open_in_memory().unwrap();
        index
            .note_scheduled_run("a", at(2026, 11, 1, 0, 0).to_utc())
            .unwrap();
        index
            .note_scheduled_run("b", at(2026, 11, 20, 0, 0).to_utc())
            .unwrap();
        // 9:00 at UTC+1 is 8:00 UTC, so this is too early in local time...
        let too_early = at(2026, 11, 10, 8, 59);
        let due = due_work(&config, &index, None, &too_early).unwrap();
        assert!(due.companies.is_empty());
        // ...even though it is already 9:00 somewhere further east.
        let east = FixedOffset::east_opt(2 * 3600).unwrap();
        let due = due_work(&config, &index, None, &too_early.with_timezone(&east)).unwrap();
        assert_eq!(due.companies, ["a"]);
    }

    #[test]
    fn several_missed_dates_are_caught_up_once() {
        let mut world = World::new();
        let config = config(ON);
        world
            .index()
            .note_scheduled_run("a", at(2026, 1, 1, 0, 0).to_utc())
            .unwrap();
        world
            .index()
            .note_scheduled_run("b", at(2026, 11, 20, 0, 0).to_utc())
            .unwrap();

        // Ten dates have passed since the last run.
        let start = at(2026, 11, 20, 12, 0);
        assert!(matches!(world.tick(&config, start), Tick::Ran(_)));
        assert_eq!(world.fetcher.started, ["a"]);
        assert_eq!(
            world.index().last_scheduled_run("a").unwrap(),
            Some(start.to_utc())
        );

        assert!(matches!(
            world.tick(&config, at(2026, 11, 20, 12, 1)),
            Tick::Idle
        ));
        assert!(matches!(
            world.tick(&config, at(2026, 12, 10, 8, 59)),
            Tick::Idle
        ));
        assert_eq!(world.fetcher.started, ["a"]);

        assert!(matches!(
            world.tick(&config, at(2026, 12, 10, 9, 0)),
            Tick::Ran(_)
        ));
        assert_eq!(world.fetcher.started, ["a", "a"]);
    }

    #[test]
    fn a_failed_scheduled_run_waits_for_the_next_date_and_others_still_run() {
        let mut world = World::new();
        let config = config(ON);
        let index = world.index();
        index
            .note_scheduled_run("a", at(2026, 11, 1, 0, 0).to_utc())
            .unwrap();
        index
            .note_scheduled_run("b", at(2026, 11, 1, 0, 0).to_utc())
            .unwrap();
        world.fetcher.failing = Some("a");

        let Tick::Ran(report) = world.tick(&config, at(2026, 11, 20, 12, 0)) else {
            panic!("expected a run");
        };

        assert_eq!(world.fetcher.started, ["a", "b"]);
        assert!(report.fetches[0].has_failures());
        assert!(!report.fetches[1].has_failures());
        assert!(matches!(
            world.tick(&config, at(2026, 11, 20, 12, 1)),
            Tick::Idle
        ));
    }

    #[test]
    fn a_run_that_cannot_start_stays_due_and_happens_right_afterwards() {
        let mut world = World::new();
        let config = config(ON);
        world
            .index()
            .note_scheduled_run("a", at(2026, 11, 1, 0, 0).to_utc())
            .unwrap();
        let now = at(2026, 11, 10, 9, 0);

        let gate = Arc::clone(&world.gate);
        let permit = gate.try_start().unwrap();
        assert!(matches!(world.tick(&config, now), Tick::Busy));
        assert!(matches!(world.tick(&config, now), Tick::Busy));
        assert!(world.fetcher.started.is_empty());
        assert_eq!(
            world.index().last_scheduled_run("a").unwrap(),
            Some(at(2026, 11, 1, 0, 0).to_utc()),
            "nothing is lost"
        );

        drop(permit);
        assert!(matches!(world.tick(&config, now), Tick::Ran(_)));
        assert_eq!(world.fetcher.started, ["a"]);
    }

    #[test]
    fn a_tick_holds_the_gate_while_it_runs_and_opens_it_afterwards() {
        struct Prober<'a> {
            gate: &'a RunGate,
            blocked: bool,
        }
        impl PortalFetcher for Prober<'_> {
            async fn fetch(&mut self, company: &Company) -> FetchReport {
                self.blocked = self.gate.try_start().is_none();
                FetchReport::failed(company, "fake".into())
            }
        }

        let world = World::new();
        let config = config(ON);
        world
            .index()
            .note_scheduled_run("a", at(2026, 11, 1, 0, 0).to_utc())
            .unwrap();
        let mut prober = Prober {
            gate: &world.gate,
            blocked: false,
        };

        block_on(run_tick(
            &config,
            &MemoryStore::default(),
            &world.index_path(),
            &world.gate,
            &mut None,
            at(2026, 11, 10, 9, 0),
            &mut prober,
            || {},
        ))
        .unwrap();

        assert!(prober.blocked, "a manual run cannot start during a tick");
        assert!(world.gate.try_start().is_some());
    }

    #[test]
    fn a_refused_permit_leaves_the_gate_closed() {
        let gate = RunGate::default();
        let permit = gate.try_start().unwrap();
        assert!(gate.try_start().is_none());
        assert!(gate.try_start().is_none());
        drop(permit);
        assert!(gate.try_start().is_some());
    }

    #[test]
    fn only_one_of_two_simultaneous_triggers_gets_to_run() {
        let gate = RunGate::default();
        let start = std::sync::Barrier::new(8);
        let everyone_tried = std::sync::Barrier::new(8);
        let won = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    start.wait();
                    let permit = gate.try_start();
                    if permit.is_some() {
                        won.fetch_add(1, Ordering::SeqCst);
                    }
                    // The winner holds its permit until every thread has tried.
                    everyone_tried.wait();
                    drop(permit);
                });
            }
        });
        assert_eq!(won.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn polling_comes_up_at_its_interval() {
        let config = with_polling(30);
        let index = Index::open_in_memory().unwrap();
        let start = at(2026, 11, 6, 12, 0);

        assert!(due_work(&config, &index, None, &start).unwrap().poll);
        let last = Some(start.to_utc());
        let early = at(2026, 11, 6, 12, 29);
        assert!(!due_work(&config, &index, last, &early).unwrap().poll);
        let late = at(2026, 11, 6, 12, 30);
        assert!(due_work(&config, &index, last, &late).unwrap().poll);
    }

    #[test]
    fn a_poll_scans_the_mailboxes_and_a_failing_mailbox_is_reported() {
        let mut world = World::new();
        let config = with_polling(30);
        let now = at(2026, 11, 6, 12, 0);

        let Tick::Ran(report) = world.tick(&config, now) else {
            panic!("expected a run");
        };

        assert!(report.accounts[0].error.is_some(), "no password is stored");
        assert!(worth_recording(&report));
        assert_eq!(world.last_poll, Some(now.to_utc()));
        assert!(matches!(
            world.tick(&config, at(2026, 11, 6, 12, 1)),
            Tick::Idle
        ));
    }

    #[test]
    fn a_company_fetched_for_a_link_mail_is_not_fetched_again_for_its_schedule() {
        let config = config(ON);
        let index = Index::open_in_memory().unwrap();
        let now = at(2026, 11, 10, 9, 0).to_utc();
        let mut report = ScanReport::default();
        report
            .fetches
            .push(FetchReport::failed(&config.companies[0], "fetched".into()));
        let mut fetcher = FakeFetcher::default();
        let due = ["a".to_owned(), "b".to_owned()];

        block_on(fetch_scheduled(
            &config,
            &index,
            &due,
            now,
            &mut report,
            &mut fetcher,
        ))
        .unwrap();

        assert_eq!(fetcher.started, ["b"]);
        assert_eq!(report.fetches.len(), 2);
        assert_eq!(index.last_scheduled_run("a").unwrap(), Some(now));
    }

    #[test]
    fn a_poll_with_nothing_to_do_is_not_worth_a_history_entry() {
        let report = ScanReport {
            accounts: vec![crate::mail::execute::AccountReport {
                account_id: "main".into(),
                mails: Vec::new(),
                error: None,
            }],
            fetches: Vec::new(),
        };
        assert!(!worth_recording(&report));
    }

    #[test]
    fn a_scheduled_fetch_is_worth_a_history_entry_even_if_nothing_was_new() {
        let config = config(ON);
        let mut report = ScanReport::default();
        report.fetches.push(FetchReport {
            company_id: "a".into(),
            company_name: config.companies[0].name.clone(),
            entries: Vec::new(),
            error: None,
        });
        assert!(worth_recording(&report));
    }
}
