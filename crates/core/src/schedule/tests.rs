use super::*;

/// A value the test does not choose by hand.
fn arbitrary(range: u64) -> u64 {
    (Id::new_v4().as_u128() % u128::from(range)) as u64
}

#[test]
fn instants_and_wall_clock_times_become_unix_seconds() {
    let seconds = 86_400 + arbitrary(4_000_000_000);
    let time = jiff::Timestamp::from_second(seconds as i64).unwrap();
    assert_eq!(parse_instant(&time.to_string()).unwrap(), seconds);
    let fraction = jiff::Timestamp::new(seconds as i64, 1 + arbitrary(999_999_998) as i32).unwrap();
    assert!(matches!(
        parse_instant(&fraction.to_string()),
        Err(ScheduleInputError::FractionalTime(_))
    ));
    assert!(matches!(
        parse_instant("tomorrow"),
        Err(ScheduleInputError::Time { .. })
    ));
    assert!(matches!(
        parse_instant("1969-12-31T23:59:59Z"),
        Err(ScheduleInputError::BeforeEpoch)
    ));

    // The same wall-clock time in two zones is the zones' offset apart.
    let minute = seconds - seconds % 60;
    let civil = time.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    let (date, clock) = (
        civil.date().to_string(),
        civil.time().strftime("%H:%M").to_string(),
    );
    assert_eq!(parse_local(&date, &clock, "UTC").unwrap(), minute);
    let fixed = jiff::tz::TimeZone::get("Asia/Kolkata").unwrap();
    let offset = fixed.to_offset(time).seconds();
    assert_eq!(
        parse_local(&date, &clock, "Asia/Kolkata").unwrap() as i64,
        minute as i64 - i64::from(offset)
    );
    assert!(matches!(
        parse_local(&date, &clock, "Nowhere/Special"),
        Err(ScheduleInputError::Zone { .. })
    ));
    assert!(matches!(
        parse_local(&date, &format!("{clock}:00.5"), "UTC"),
        Err(ScheduleInputError::FractionalTime(_))
    ));
    assert!(matches!(
        parse_local("someday", &clock, "UTC"),
        Err(ScheduleInputError::Local { .. })
    ));
}

#[test]
fn clock_changes_move_skipped_times_forward_and_take_repeated_ones_late() {
    let zone = "America/New_York";
    // 02:30 on 2026-03-08 does not exist there: clocks jump 02:00 → 03:00,
    // so it is read as 03:30, one hour after 01:30.
    let skipped = parse_local("2026-03-08", "02:30", zone).unwrap();
    let before = parse_local("2026-03-08", "01:30", zone).unwrap();
    assert_eq!(skipped - before, 3_600);
    assert_eq!(parse_local("2026-03-08", "03:30", zone).unwrap(), skipped);
    // 01:30 on 2026-11-01 happens twice; the second one is two hours after
    // 00:30.
    let repeated = parse_local("2026-11-01", "01:30", zone).unwrap();
    let before = parse_local("2026-11-01", "00:30", zone).unwrap();
    assert_eq!(repeated - before, 7_200);
}

#[test]
fn intervals_are_positive_whole_seconds_and_print_as_typed() {
    let seconds = 1 + arbitrary(10_000_000);
    let written = format_interval(seconds);
    assert_eq!(parse_interval(&written).unwrap(), seconds, "{written}");
    assert_eq!(parse_interval(&format!("{seconds}s")).unwrap(), seconds);
    assert!(matches!(
        parse_interval(&format!("{seconds}.5s")),
        Err(ScheduleInputError::Fraction(_))
    ));
    for zero_or_less in ["0s", "-5m"] {
        assert!(matches!(
            parse_interval(zero_or_less),
            Err(ScheduleInputError::NotPositive)
        ));
    }
    assert!(matches!(
        parse_interval("soon"),
        Err(ScheduleInputError::Duration { .. })
    ));
}

#[test]
fn days_and_weeks_are_fixed_lengths_and_months_are_refused() {
    let days = 1 + arbitrary(1_000);
    let hours = arbitrary(24);
    assert_eq!(
        parse_interval(&format!("{days}d {hours}h")).unwrap(),
        (days * 24 + hours) * 3600
    );
    assert_eq!(
        parse_interval(&format!("{days}w")).unwrap(),
        days * 7 * 24 * 3600
    );
    for calendar in ["1mo", "1y"] {
        assert!(matches!(
            parse_interval(calendar),
            Err(ScheduleInputError::Duration { .. })
        ));
    }
}
