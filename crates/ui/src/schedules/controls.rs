//! Domain controls for schedules; the gateway remains the validation authority.
use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum IntervalUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
}
impl IntervalUnit {
    const ALL: [Self; 5] = [
        Self::Seconds,
        Self::Minutes,
        Self::Hours,
        Self::Days,
        Self::Weeks,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::Seconds => "Seconds",
            Self::Minutes => "Minutes",
            Self::Hours => "Hours",
            Self::Days => "Days",
            Self::Weeks => "Weeks",
        }
    }
    pub(super) fn duration(self, amount: &str) -> Option<String> {
        let amount = amount.trim();
        if amount.is_empty()
            || !amount.bytes().all(|byte| byte.is_ascii_digit())
            || amount.parse::<u64>().ok()? == 0
        {
            return None;
        }
        // Match the gateway's minimum interval; retain second precision for
        // durations such as 90 seconds without accepting a faster schedule.
        if self == Self::Seconds && amount.parse::<u64>().ok()? < 60 {
            return None;
        }
        let suffix = match self {
            Self::Seconds => "s",
            Self::Minutes => "m",
            Self::Hours => "h",
            Self::Days => "d",
            Self::Weeks => "w",
        };
        Some(format!("{amount}{suffix}"))
    }
}

pub(super) const WEEKDAYS: [(u8, &str); 7] = [
    (1, "Monday"),
    (2, "Tuesday"),
    (3, "Wednesday"),
    (4, "Thursday"),
    (5, "Friday"),
    (6, "Saturday"),
    (0, "Sunday"),
];

/// Generated cron has minute precision and explicit weekdays. Reject incomplete
/// calendar values instead of silently turning them into a different schedule.
pub(super) fn calendar_expression(time: &str, days: Option<&[u8]>) -> Option<String> {
    let clock = time.parse::<jiff::civil::Time>().ok()?;
    if clock.second() != 0 || clock.subsec_nanosecond() != 0 {
        return None;
    }
    let weekdays = match days {
        None => "*".to_owned(),
        Some([]) => return None,
        Some(days) if days.iter().all(|day| *day <= 6) => {
            let mut days = days.to_vec();
            days.sort_unstable();
            days.dedup();
            days.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        }
        Some(_) => return None,
    };
    Some(format!(
        "{} {} * * {weekdays}",
        clock.minute(),
        clock.hour()
    ))
}

#[component]
pub(super) fn IntervalFields(amount: Signal<String>, unit: Signal<IntervalUnit>) -> Element {
    let selected = use_memo(move || Some(unit().label().to_owned()));
    rsx! {
        Field { label: rsx! { "Repeat every" }, hint: rsx! { "At least one minute, counted from when the gateway accepts the schedule. Days and weeks are fixed durations." }, error: unit().duration(&amount()).is_none().then(|| rsx! { "Choose an interval of at least one minute, using a whole number." }),
            Input { aria_label: "Interval amount", r#type: InputType::Number, min: if unit() == IntervalUnit::Seconds { "60" } else { "1" }, step: "1", value: ReadSignal::from(amount), oninput: move |v| amount.set(v) }
        }
        Field { label: rsx! { "Unit" },
            Select { label: name!("Interval unit"), value: ReadSignal::from(selected),
                options: IntervalUnit::ALL.into_iter().map(|u| SelectChoice::new(u.label(), u.label())).collect(),
                onchange: move |v| if let Some(chosen) = IntervalUnit::ALL.into_iter().find(|u| u.label() == v) { unit.set(chosen); }
            }
        }
    }
}

#[component]
pub(super) fn WeekdayChoice(day: u8, label: &'static str, days: Signal<Vec<u8>>) -> Element {
    let checked = use_memo(move || {
        if days.read().contains(&day) {
            CheckState::On
        } else {
            CheckState::Off
        }
    });
    rsx! { Checkbox { label: rsx! { "{label}" }, checked: ReadSignal::from(checked), onchange: move |on| {
        let mut chosen = days.write();
        chosen.retain(|value| *value != day);
        if on { chosen.push(day); }
    } } }
}

#[component]
pub(super) fn ZoneSelector(mut zone: Signal<String>, gateway_default: bool) -> Element {
    let choices = use_hook(|| {
        jiff::tz::db()
            .available()
            .map(|name| {
                let name = name.to_string();
                SelectChoice::new(&name, &name)
            })
            .collect::<Vec<_>>()
    });
    let selected = use_memo(move || Some(zone()));
    let mut options = choices;
    if gateway_default {
        options.insert(0, SelectChoice::new("", "Gateway time zone"));
    }
    let current = zone();
    if !current.is_empty() && !options.iter().any(|option| option.value == current) {
        // A system zone can be usable even when the local database cannot list
        // it. Preserve that explicit choice; never substitute another zone.
        options.insert(0, SelectChoice::new(&current, &current));
    }
    rsx! { Field { label: rsx! { "Time zone" }, hint: rsx! { "Type a region or city while the list is open to jump to it." },
        Select { label: name!("Schedule time zone"), value: ReadSignal::from(selected), options,
            placeholder: "Choose time zone", onchange: move |value| zone.set(value)
        }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_controls_generate_exact_wall_clock_and_weekdays() {
        assert_eq!(
            calendar_expression("09:25", None).as_deref(),
            Some("25 9 * * *")
        );
        assert_eq!(
            calendar_expression("18:05", Some(&[5, 1, 5, 0])).as_deref(),
            Some("5 18 * * 0,1,5")
        );
        for (time, days) in [
            ("", None),
            ("25:00", None),
            ("09:00:01", None),
            ("09:00:00.5", None),
            ("09:00", Some(&[][..])),
            ("09:00", Some(&[7][..])),
        ] {
            assert_eq!(calendar_expression(time, days), None);
        }
    }
    #[test]
    fn units_feed_existing_validation_without_truncation() {
        assert_eq!(IntervalUnit::Seconds.duration("59"), None);
        assert_eq!(
            parse_interval(&IntervalUnit::Seconds.duration("90").unwrap()).unwrap(),
            90
        );
        for (unit, seconds) in [
            (IntervalUnit::Minutes, 120),
            (IntervalUnit::Hours, 7200),
            (IntervalUnit::Days, 172800),
            (IntervalUnit::Weeks, 1209600),
        ] {
            assert_eq!(
                parse_interval(&unit.duration("2").unwrap()).unwrap(),
                seconds
            );
            for amount in ["-1", "0", "1e9", "1.5", "1h 5", "18446744073709551616"] {
                assert_eq!(unit.duration(amount), None);
            }
        }
    }
}
