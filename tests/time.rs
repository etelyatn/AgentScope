use agentscope::time::{format_absolute, relative_label};

#[test]
fn relative_labels() {
    let now = 10_000_000;
    assert_eq!(relative_label(now - 12_000, now), "12s");
    assert_eq!(relative_label(now - 9 * 60_000, now), "9m");
    assert_eq!(relative_label(now - 2 * 3_600_000, now), "2h");
    assert_eq!(relative_label(now - 86_400_000, now), "1d");
    assert_eq!(relative_label(now + 5_000, now), "0s");
}

#[test]
fn absolute_uses_explicit_offset_including_dst_change() {
    // 2026-03-08T12:00:00Z: US Eastern is already on EDT (-4h); a month earlier it was EST (-5h).
    let summer = 1_772_971_200_000;
    assert_eq!(
        format_absolute(summer, -4 * 3600),
        "2026-03-08 08:00:00 -04:00"
    );
    let winter = summer - 30 * 86_400_000;
    assert_eq!(
        format_absolute(winter, -5 * 3600),
        "2026-02-06 07:00:00 -05:00"
    );
    assert_eq!(format_absolute(0, 0), "1970-01-01 00:00:00 +00:00");
}
