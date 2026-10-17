use super::*;

/// A Unix time in this century, generated at runtime.
fn time() -> u64 {
    1_000_000_000 + (uuid::Uuid::new_v4().as_u128() % 3_000_000_000) as u64
}
fn utc(expression: &str) -> Schedule {
    Schedule::Cron {
        expression: expression.into(),
        zone: Some("UTC".into()),
    }
}

#[test]
fn one_time_jobs_are_due_once_and_never_in_the_past() {
    let at = time();
    let schedule = Schedule::At { at };
    assert_eq!(next_after(&schedule, at - 1).unwrap(), Some(at));
    assert_eq!(next_after(&schedule, at).unwrap(), None);
    assert!(matches!(first(&schedule, at), Err(ScheduleError::Past)));
    assert_eq!(first(&schedule, at - 1).unwrap(), at);
}

#[test]
fn intervals_stay_on_their_anchored_grid() {
    let anchor = time();
    let seconds = MIN_INTERVAL + time() % 86_400;
    let schedule = Schedule::Every { seconds, anchor };
    let steps = time() % 1000;
    let late = time() % seconds;
    assert_eq!(
        next_after(&schedule, anchor + steps * seconds + late).unwrap(),
        Some(anchor + (steps + 1) * seconds)
    );
    assert_eq!(next_after(&schedule, anchor - 1).unwrap(), Some(anchor));
    for seconds in [MIN_INTERVAL - 1, MAX_INTERVAL + 1] {
        assert!(matches!(
            next_after(&Schedule::Every { seconds, anchor }, anchor),
            Err(ScheduleError::Interval)
        ));
    }
}

#[test]
fn cron_expressions_follow_their_zone_with_optional_seconds() {
    let now = time();
    assert_eq!(
        next_after(&utc("0 * * * *"), now).unwrap(),
        Some((now / 3600 + 1) * 3600)
    );
    assert_eq!(
        next_after(&utc("*/30 * * * * *"), now).unwrap(),
        Some((now / 30 + 1) * 30)
    );
    assert!(matches!(
        next_after(&utc(&uuid::Uuid::new_v4().to_string()), now),
        Err(ScheduleError::Expression(_))
    ));
    let zone = uuid::Uuid::new_v4().simple().to_string();
    assert!(matches!(
        next_after(
            &Schedule::Cron {
                expression: "0 * * * *".into(),
                zone: Some(zone),
            },
            now
        ),
        Err(ScheduleError::Zone { .. })
    ));
}
