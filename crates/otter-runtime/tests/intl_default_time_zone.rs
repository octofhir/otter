//! `Intl.DateTimeFormat` with no `timeZone` formats in the host's zone —
//! the one `Date` reads local time in — and says which (ECMA-402
//! DefaultTimeZone).
//!
//! It used to compute wall-clock time in the host zone but name it UTC
//! ("3:00 PM, UTC" for noon UTC in Moscow), and `resolvedOptions()` said
//! UTC, so a page comparing either with `Date` disagreed with itself.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "https://site.test/run.js",
        )
        .expect("script runs")
        .completion_string()
        .to_owned()
}

#[test]
fn the_default_zone_is_the_one_date_uses() {
    let result = run(r#"
        const out = [];
        const zone = new Intl.DateTimeFormat('en-US').resolvedOptions().timeZone;
        for (const ms of [Date.UTC(2026, 0, 15, 12, 0), Date.UTC(2026, 6, 15, 12, 0)]) {
            const d = new Date(ms);
            const hour = new Intl.DateTimeFormat('en-US', { hour: 'numeric', hourCycle: 'h23' }).format(d);
            if (Number(hour) !== d.getHours()) out.push('hour ' + hour + ' vs ' + d.getHours());
            const options = { hour: 'numeric', minute: '2-digit', timeZoneName: 'short' };
            const implicit = d.toLocaleTimeString('en-US', options);
            const explicit = d.toLocaleTimeString('en-US', { ...options, timeZone: zone });
            if (implicit !== explicit) out.push(implicit + ' vs ' + explicit);
        }
        out.join('; ') || 'consistent'
    "#);
    assert_eq!(result, "consistent");
}
