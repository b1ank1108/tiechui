use super::*;
use cron::{Local, days_from_civil, utc};
use tiechui_plugin_api::protocol::TurnStatus;

const DAY: i64 = 1440 * cron::MINUTE;

struct Fixed(i64);
impl Zone for Fixed {
    fn local(&self, ms: i64) -> Local {
        Local {
            offset_minutes: self.0,
            ..utc(ms + self.0 * cron::MINUTE)
        }
    }
    fn resolve(&self, year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64> {
        Some(wall(year, month, day, hour, minute) - self.0 * cron::MINUTE)
    }
}

/// UTC−5 with UTC−4 daylight time from 2026-03-08 07:00Z to 2026-11-01 06:00Z.
struct Eastern;
impl Eastern {
    fn offset(ms: i64) -> i64 {
        let start = wall(2026, 3, 8, 7, 0);
        let end = wall(2026, 11, 1, 6, 0);
        if (start..end).contains(&ms) {
            -240
        } else {
            -300
        }
    }
}
impl Zone for Eastern {
    fn local(&self, ms: i64) -> Local {
        Fixed(Self::offset(ms)).local(ms)
    }
    fn resolve(&self, year: i64, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64> {
        let at = wall(year, month, day, hour, minute);
        [-240, -300]
            .map(|offset| at - offset * cron::MINUTE)
            .into_iter()
            .filter(|&ms| Self::offset(ms) * cron::MINUTE == at - ms)
            .min()
    }
}

fn wall(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    days_from_civil(year, month, day) * DAY + i64::from(hour * 60 + minute) * cron::MINUTE
}

fn next(cron: &str, after: i64, zone: &dyn Zone) -> Option<i64> {
    Cron::parse(cron).unwrap().next(after, zone)
}

#[test]
fn cron_fields_ranges_steps_lists_and_weekday_seven() {
    for invalid in [
        "* * * *",
        "60 * * * *",
        "* 24 * * *",
        "* * 0 * *",
        "* * * 13 *",
        "* * * * 8",
        "5/10 * * * *",
        "*/0 * * * *",
        "L * * * *",
        "1- * * * *",
        "* * * * MON",
        "5-1 * * * *",
        "* * * * * *",
    ] {
        assert!(Cron::parse(invalid).is_err(), "{invalid}");
    }
    let utc = Fixed(0);
    let monday = wall(2026, 11, 2, 0, 0);
    assert_eq!(
        next("*/15 9-17/4 * * *", monday, &utc),
        Some(wall(2026, 11, 2, 9, 0))
    );
    assert_eq!(
        next("10,20 13 * * *", wall(2026, 11, 2, 13, 10), &utc),
        Some(wall(2026, 11, 2, 13, 20))
    );
    assert_eq!(
        next("0 9 * * 7", monday, &utc),
        Some(wall(2026, 11, 8, 9, 0))
    );
    assert_eq!(
        next("0 9 * * 0", monday, &utc),
        Some(wall(2026, 11, 8, 9, 0))
    );
    assert_eq!(
        next("0 9 * * 1-5", wall(2026, 11, 6, 10, 0), &utc),
        Some(wall(2026, 11, 9, 9, 0))
    );
}

#[test]
fn next_is_strictly_later_and_bounded_to_a_year() {
    let utc = Fixed(0);
    let at = wall(2026, 11, 2, 12, 0);
    assert_eq!(next("* * * * *", at, &utc), Some(at + cron::MINUTE));
    assert_eq!(next("0 12 * * *", at, &utc), Some(at + DAY));
    assert_eq!(next("0 12 * * *", at - 1, &utc), Some(at));
    // Day of month and day of week both restricted: either matches.
    assert_eq!(
        next("0 0 13 * 5", wall(2026, 11, 2, 0, 0), &utc),
        Some(wall(2026, 11, 6, 0, 0))
    );
    assert_eq!(
        next("0 0 13 * 5", wall(2026, 11, 7, 0, 0), &utc),
        Some(wall(2026, 11, 13, 0, 0))
    );
    assert_eq!(next("0 0 30 2 *", at, &utc), None);
    assert_eq!(next("0 0 29 2 *", at, &utc), None);
    assert_eq!(
        next("0 0 29 2 *", wall(2027, 6, 1, 0, 0), &utc),
        Some(wall(2028, 2, 29, 0, 0))
    );
    assert_eq!(next("0 0 2 11 *", at, &utc), Some(wall(2027, 11, 2, 0, 0)));
}

#[test]
fn local_time_skips_missing_and_runs_repeated_wall_times_once() {
    let spring = next("30 2 * * *", wall(2026, 3, 8, 5, 0), &Eastern).unwrap();
    assert_eq!(Eastern.local(spring).day, 9);
    let first = next("30 1 * * *", wall(2026, 11, 1, 4, 0), &Eastern).unwrap();
    assert_eq!(first, wall(2026, 11, 1, 5, 30));
    let second = next("30 1 * * *", first, &Eastern).unwrap();
    assert_eq!(
        (Eastern.local(second).day, Eastern.local(second).hour),
        (2, 1)
    );
    let mut at = wall(2026, 11, 1, 4, 45);
    let mut seen = vec![];
    for _ in 0..3 {
        at = next("*/30 * * * *", at, &Eastern).unwrap();
        let local = Eastern.local(at);
        seen.push((local.hour, local.minute, local.offset_minutes));
    }
    assert_eq!(seen, [(1, 0, -240), (1, 30, -240), (2, 0, -300)]);
    let india = Fixed(330);
    assert_eq!(
        next("0 9 * * *", wall(2026, 11, 2, 0, 0), &india),
        Some(wall(2026, 11, 2, 3, 30))
    );
    assert_eq!(cron::format(0, &india), "1970-01-01 05:30 +05:30");
    assert_eq!(cron::format(0, &Fixed(-210)), "1969-12-31 20:30 -03:30");
}

#[test]
fn jitter_is_stable_bounded_and_never_before_creation() {
    let utc = Fixed(0);
    let at = wall(2026, 11, 2, 12, 3);
    let every = |cron: &str, id: &str| {
        cron::recurring_fire(&Cron::parse(cron).unwrap(), at, id, &utc).unwrap()
    };
    assert_eq!(every("*/10 * * * *", "00000000"), wall(2026, 11, 2, 12, 10));
    assert_eq!(
        every("*/10 * * * *", "80000000"),
        wall(2026, 11, 2, 12, 10) + 30_000
    );
    assert_eq!(
        every("0 * * * *", "80000000"),
        wall(2026, 11, 2, 13, 0) + 3 * cron::MINUTE
    );
    assert_eq!(
        every("0 9 * * *", "ffffffff"),
        wall(2026, 11, 3, 9, 0) + 15 * cron::MINUTE
    );
    assert_eq!(every("*/10 * * * *", "zzzzzzzz"), wall(2026, 11, 2, 12, 10));
    let once = |cron: &str, created: i64| {
        cron::one_shot_fire(&Cron::parse(cron).unwrap(), created, "80000000", &utc).unwrap()
    };
    assert_eq!(once("30 14 2 11 *", at), wall(2026, 11, 2, 14, 30) - 45_000);
    assert_eq!(once("31 14 2 11 *", at), wall(2026, 11, 2, 14, 31));
    let late = wall(2026, 11, 2, 14, 29) + 50_000;
    assert_eq!(once("30 14 2 11 *", late), late);
}

#[test]
fn intervals_round_to_the_nearest_even_period() {
    let cron = |amount, unit| cron::interval(amount, unit).map(|i| (i.cron, i.minutes));
    assert_eq!(cron(5, 'm'), Some(("*/5 * * * *".into(), 5)));
    assert_eq!(cron(30, 's'), Some(("* * * * *".into(), 1)));
    assert_eq!(cron(7, 'm'), Some(("*/6 * * * *".into(), 6)));
    assert_eq!(cron(45, 'm'), Some(("0 * * * *".into(), 60)));
    assert_eq!(cron(90, 'm'), Some(("0 */2 * * *".into(), 120)));
    assert_eq!(cron(5, 'h'), Some(("0 */6 * * *".into(), 360)));
    assert_eq!(cron(36, 'h'), Some(("0 0 */2 * *".into(), 2880)));
    assert_eq!(cron(1, 'd'), Some(("0 0 * * *".into(), 1440)));
    assert_eq!(cron(31, 'd'), Some(("0 0 */31 * *".into(), 44640)));
    for (amount, unit) in [(0, 'm'), (32, 'd'), (u64::MAX, 'd'), (5, 'w')] {
        assert_eq!(cron(amount, unit), None);
    }
}

#[test]
fn loop_arguments_take_a_leading_or_trailing_interval_and_keep_the_prompt() {
    let parse = |text| cron::request(text).map(|r| (r.amount, r.unit, r.prompt));
    assert_eq!(
        parse("5m check the deploy"),
        Some((5, 'm', "check the deploy"))
    );
    assert_eq!(
        parse("check the deploy every 20m"),
        Some((20, 'm', "check the deploy"))
    );
    assert_eq!(
        parse("run tests every 5 Minutes"),
        Some((5, 'm', "run tests"))
    );
    assert_eq!(parse("check every PR"), Some((10, 'm', "check every PR")));
    assert_eq!(
        parse("1h  first line\n  \"second\" $skill  "),
        Some((1, 'h', "first line\n  \"second\" $skill"))
    );
    for usage in ["", "5m", "every 5m", "99999999999999999999999m check"] {
        assert_eq!(parse(usage), None, "{usage}");
    }
}

#[test]
fn schedules_read_as_words_when_common() {
    for (cron, text) in [
        ("* * * * *", "every minute"),
        ("*/5 * * * *", "every 5 minutes"),
        ("0 * * * *", "every hour"),
        ("7 * * * *", "every hour at :07"),
        ("0 */2 * * *", "every 2 hours"),
        ("57 8 * * *", "every day at 08:57"),
        ("0 9 * * 1-5", "weekdays at 09:00"),
        ("0 9 * * 1", "every Monday at 09:00"),
        ("30 14 3 11 *", "on Nov 3 at 14:30"),
        ("0 0 */2 * *", "at 00:00 every 2 days of the month"),
        ("1-5 * * * *", "cron 1-5 * * * *"),
    ] {
        assert_eq!(cron::describe(cron), text);
    }
}

fn task(id: &str, cron: &str, created_at: i64, recurring: bool) -> Task {
    Task {
        id: id.into(),
        cron: cron.into(),
        prompt: format!("prompt {id}"),
        created_at,
        last_fired_at: None,
        recurring,
    }
}

#[test]
fn runs_advance_from_now_expire_after_a_week_and_missed_questions_come_first() {
    let utc = Fixed(0);
    let created = wall(2026, 11, 2, 12, 0);
    let mut s = State::default();
    s.tasks.push(Entry::new(
        task("00000000", "*/5 * * * *", created, true),
        false,
        &utc,
    ));
    s.tasks.push(Entry::new(
        task("00000001", "*/5 * * * *", created, true),
        false,
        &utc,
    ));
    assert!(s.due(created, &utc).is_none());
    // Several missed periods run once, earliest creation first.
    let late = created + 20 * cron::MINUTE + 1;
    let first = s.due(late, &utc).unwrap();
    assert_eq!((first.identity.as_str(), first.visible), ("00000000", true));
    s.fired("00000000", late, &utc);
    assert_eq!(s.running.as_deref(), Some("00000000"));
    assert_eq!(s.tasks[0].due, Some(created + 25 * cron::MINUTE));
    assert_eq!(s.due(late, &utc).unwrap().identity, "00000001");
    let expired = created + MAX_AGE;
    s.fired("00000001", expired, &utc);
    assert_eq!(s.tasks.len(), 1);
    s.missed
        .push(task("00000002", "0 9 1 11 *", created, false));
    let question = s.due(late, &utc).unwrap();
    assert!(!question.visible && question.identity.is_empty());
    assert!(question.text.contains("Do not run this prompt yet"));
    assert!(question.text.contains("```\nprompt 00000002\n```"));
    s.fired("", late, &utc);
    assert!(s.missed.is_empty());
    assert_eq!(s.writes, [Write::Remove("00000002".into())]);
}

#[test]
fn durable_views_keep_unwritten_runs_and_owners_set_aside_missed_one_shots() {
    let utc = Fixed(0);
    let created = wall(2026, 11, 2, 12, 0);
    let mut s = State::default();
    let recurring = task("00000000", "*/5 * * * *", created, true);
    let once = task("00000001", "10 12 2 11 *", created, false);
    s.load(vec![recurring.clone(), once.clone()], &utc);
    assert!(s.due(created + DAY, &utc).is_none(), "not the owner");
    s.owner = Some(tempfile::tempfile().unwrap());
    s.owned_since = created + DAY;
    s.load(vec![recurring.clone(), once.clone()], &utc);
    assert_eq!(s.missed, [once]);
    s.missed.clear();
    s.write(Write::Fired("00000000".into(), created + DAY));
    s.write(Write::Fired("00000000".into(), created + DAY + 1));
    assert_eq!(s.writes.len(), 1);
    s.load(vec![recurring], &utc);
    assert_eq!(s.tasks[0].task.last_fired_at, Some(created + DAY + 1));
    assert_eq!(
        s.status(created + DAY, &utc).unwrap(),
        "1 scheduled task · next 12:05"
    );
}

#[test]
fn commands_are_rejected_and_empty_prompts_are_not_scheduled() {
    assert!(check("check the deploy").is_ok());
    assert!(check("/tmp/build.log looks wrong").is_ok());
    assert!(check("use $review-build").is_ok());
    assert!(
        check("/review-build now")
            .unwrap_err()
            .contains("$review-build")
    );
    assert!(check("  \n ").is_err());
}

fn module(root: Option<PathBuf>, schedule: bool) -> Arc<Loop> {
    Arc::new(Loop {
        schedules: Schedules::default(),
        store: root.map(Store::new),
        schedule,
        driver: CancellationToken::new(),
        started: AtomicBool::new(false),
    })
}

fn spec(cron: &str, durable: bool) -> Spec {
    Spec {
        cron: cron.into(),
        prompt: "check the build".into(),
        recurring: true,
        durable,
        now: false,
    }
}

fn branch(session: &str) -> BranchView {
    BranchView {
        session_id: session.into(),
        leaf: "leaf".into(),
        cwd: PathBuf::from("/test"),
        parent_session: None,
        entries: vec![],
    }
}

#[tokio::test]
async fn loop_runs_now_blocks_nested_creation_and_lives_with_its_session() {
    let inner = module(None, true);
    inner
        .event(&RuntimeEvent::Restore { navigation: false }, &branch("a"))
        .unwrap();
    let command = LoopCommand {
        inner: inner.clone(),
        name: "loop",
    };
    let usage = command.run("5m", CancellationToken::new()).await;
    assert!(usage.unwrap_err().0.starts_with("Usage: /loop"));
    let text = command
        .run("7m check the build", CancellationToken::new())
        .await
        .unwrap();
    assert!(text.contains("every 6 minutes (`*/6 * * * *` local time, 7m rounded"));
    let next = inner.continuation().unwrap();
    assert_eq!(
        (next.text.as_str(), next.visible),
        ("check the build", true)
    );
    assert!(inner.accepts(&next));
    inner
        .event(
            &RuntimeEvent::Input {
                continuation: Some(next.clone()),
                retry: false,
            },
            &branch("a"),
        )
        .unwrap();
    assert!(!inner.accepts(&next));
    assert!(inner.continuation().is_none());
    let nested = inner.create(spec("*/5 * * * *", false), CancellationToken::new());
    assert!(
        nested
            .await
            .err()
            .unwrap_or_default()
            .contains("scheduled run")
    );
    inner
        .event(&RuntimeEvent::Settled(TurnStatus::Completed), &branch("a"))
        .unwrap();
    inner
        .create(spec("*/5 * * * *", false), CancellationToken::new())
        .await
        .unwrap();
    inner
        .event(&RuntimeEvent::Restore { navigation: false }, &branch("a"))
        .unwrap();
    assert_eq!(inner.list().await.unwrap().len(), 2);
    inner
        .event(&RuntimeEvent::Restore { navigation: false }, &branch("b"))
        .unwrap();
    assert!(inner.list().await.unwrap().is_empty());
    let created = inner.create(spec("*/5 * * * *", true), CancellationToken::new());
    assert!(
        created
            .await
            .err()
            .unwrap_or_default()
            .contains("trusted project")
    );
    let exec = module(None, false);
    let session = exec.create(spec("*/5 * * * *", false), CancellationToken::new());
    assert!(
        session
            .await
            .err()
            .unwrap_or_default()
            .contains("interactive session")
    );
}

#[tokio::test]
async fn durable_tasks_are_locked_counted_and_never_overwrite_a_bad_file() {
    let root = tempfile::tempdir().unwrap();
    let inner = module(Some(root.path().into()), true);
    let path = root.path().join(store::FILE);
    let created = inner
        .create(spec("7 9 * * *", true), CancellationToken::new())
        .await
        .unwrap();
    let saved: Value =
        tiechui_plugin_api::serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["tasks"][0]["id"], created.task.id.as_str());
    assert!(inner.lock().unwrap().owner.is_some());
    let other = Store::new(root.path().into());
    assert!(other.claim().is_none());
    let listed = inner.list().await.unwrap();
    assert!(listed[0].durable && listed[0].due.is_some());
    inner
        .delete(&created.task.id, CancellationToken::new())
        .await
        .unwrap();
    assert!(!path.exists());
    let missing = inner.delete("00000000", CancellationToken::new()).await;
    assert!(missing.unwrap_err().contains("No scheduled task"));
    std::fs::write(&path, "{\"version\":2,\"tasks\":[]}").unwrap();
    let rejected = inner.create(spec("7 9 * * *", true), CancellationToken::new());
    assert!(
        rejected
            .await
            .err()
            .unwrap_or_default()
            .contains("unsupported version")
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"version\":2,\"tasks\":[]}"
    );
    std::fs::remove_file(&path).unwrap();
    other
        .update(&CancellationToken::new(), |tasks| {
            for index in 0..MAX_TASKS - 5 {
                tasks.push(task(&format!("{index:08x}"), "* * * * *", 0, true));
            }
            Ok(())
        })
        .unwrap();
    let writers: Vec<_> = (0..10)
        .map(|_| {
            let store = Store::new(root.path().into());
            std::thread::spawn(move || {
                store.update(&CancellationToken::new(), |tasks| {
                    if tasks.len() >= MAX_TASKS {
                        return Err("full".to_owned());
                    }
                    let id = fresh(|id| tasks.iter().any(|t| t.id == id));
                    tasks.push(task(&id, "* * * * *", 0, true));
                    Ok(())
                })
            })
        })
        .collect();
    let written = writers
        .into_iter()
        .filter_map(|writer| writer.join().unwrap().ok())
        .count();
    assert_eq!(written, 5);
    assert_eq!(other.read().unwrap().len(), MAX_TASKS);
    let full = inner.create(spec("7 9 * * *", true), CancellationToken::new());
    assert!(full.await.err().unwrap_or_default().contains("Too many"));
}

async fn eventually<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(value) = probe() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not reached in 5s");
}

#[tokio::test]
async fn the_owner_runs_durable_tasks_and_records_the_run() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::new(root.path().into());
    let at = now();
    store
        .update(&CancellationToken::new(), |tasks| {
            tasks.push(task("00000000", "* * * * *", at - 2 * DAY, true));
            tasks.push(task("00000001", "* * * * *", at - DAY, false));
            Ok(())
        })
        .unwrap();
    let inner = module(Some(root.path().into()), true);
    inner
        .event(&RuntimeEvent::Restore { navigation: false }, &branch("a"))
        .unwrap();
    let question = eventually(|| inner.continuation()).await;
    assert!(!question.visible && question.text.contains("prompt 00000001"));
    for _ in 0..2 {
        let next = inner.continuation().unwrap();
        inner
            .event(
                &RuntimeEvent::Input {
                    continuation: Some(next),
                    retry: false,
                },
                &branch("a"),
            )
            .unwrap();
        inner
            .event(&RuntimeEvent::Settled(TurnStatus::Completed), &branch("a"))
            .unwrap();
    }
    let saved = eventually(|| {
        let tasks = store.read().unwrap();
        (tasks.len() == 1 && tasks[0].last_fired_at.is_some()).then_some(tasks)
    })
    .await;
    assert_eq!(saved[0].id, "00000000");
    inner.event(&RuntimeEvent::Shutdown, &branch("a")).unwrap();
    assert!(store.claim().is_some());
}
