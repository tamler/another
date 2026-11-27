use another_core::{parse_duration, Since};

#[test]
fn duration_units_and_bare_seconds_policy() {
    for (text, seconds) in [
        ("0s", 0),
        (" 90s ", 90),
        ("15m", 900),
        ("2h", 7200),
        ("1d", 86_400),
        ("1w", 604_800),
    ] {
        assert_eq!(parse_duration(text, true).unwrap(), seconds);
        assert_eq!(parse_duration(text, false).unwrap(), seconds);
        assert_eq!(
            Since::parse(text, 1_000_000_000).unwrap(),
            Since::Time(1_000_000_000 - seconds * 1000)
        );
    }
    assert_eq!(parse_duration(" 90 ", true).unwrap(), 90);
    assert!(parse_duration("90", false).is_err());
    assert_eq!(Since::parse("90", 1_000_000).unwrap(), Since::Time(910_000));
    assert_eq!(
        Since::parse("0", 1_000_000).unwrap(),
        Since::Time(1_000_000)
    );
    assert_eq!(
        Since::parse(" 90 ", 1_000_000).unwrap(),
        Since::Time(910_000)
    );
    assert_eq!(
        Since::parse("1970-01-01T00:00:01Z", 1_000_000).unwrap(),
        Since::Time(1_000)
    );
    assert_eq!(
        Since::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV", 1_000_000).unwrap(),
        Since::After("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())
    );
}

#[test]
fn duration_validation_and_overflow_keep_seconds_and_milliseconds_distinct() {
    assert!(Since::parse("nonsense", 0).unwrap_err().to_string().contains(
        "invalid since \"nonsense\": use a duration like 3600 (bare seconds), 90m or 2h, an RFC 3339 time, or an event id"
    ));
    for text in ["", "-1s", "+1s", "1.5h", "1H", "1ms", "s", "1 s", "💡"] {
        assert!(parse_duration(text, true).is_err(), "{text:?}");
        assert!(parse_duration(text, false).is_err(), "{text:?}");
    }
    assert_eq!(
        parse_duration("9223372036854775807s", false).unwrap(),
        i64::MAX
    );
    assert!(parse_duration("9223372036854775807m", true).is_err());
    assert!(parse_duration("9223372036854775808s", true).is_err());
    assert!(Since::parse("9223372036854775807s", 0).is_err());
    assert!(Since::parse("9223372036854775807", 0).is_err());
}
