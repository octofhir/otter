//! Moving-GC invariants for Intl result builders.
//!
//! # Contents
//! - Every formatter `formatToParts`/range builder plus `resolvedOptions`.
//! - `Intl.Locale` string-array and locale-info object builders.
//! - Static `Intl.getCanonicalLocales` array construction and mutation.
//!
//! # Invariants
//! - Each part object, string field, optional `source`/`unit`, and final array
//!   remains current when every allocation triggers a moving collection.
//! - Builders retain canonical rooted handles rather than tracing a copied
//!   snapshot while continuing to mutate stale raw values.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str, name: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source.to_string()), name)
        .expect("Intl builder fixture")
        .completion_string()
        .to_owned()
}

#[test]
fn intl_parts_and_resolved_options_survive_moving_gc() {
    let completion = run(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 24; i++) {
                tail = { seed, i, text: "intl-" + seed + "-" + i, tail };
            }
            return tail;
        }

        function validateParts(parts, label) {
            const held = parts;
            const allocated = churn(label.length);
            if (held !== parts || allocated.seed !== label.length || !Array.isArray(held)) {
                throw new Error(label + " array roots");
            }
            if (held.length === 0) throw new Error(label + " emitted no parts");
            let text = "";
            for (let i = 0; i < held.length; i++) {
                const part = held[i];
                churn(100 + i);
                if (
                    typeof part !== "object" ||
                    typeof part.type !== "string" ||
                    typeof part.value !== "string"
                ) {
                    throw new Error(label + " part roots");
                }
                if ("source" in part && typeof part.source !== "string") {
                    throw new Error(label + " source roots");
                }
                if ("unit" in part && typeof part.unit !== "string") {
                    throw new Error(label + " unit roots");
                }
                text += part.value;
            }
            return held.length + text.length;
        }

        const number = new Intl.NumberFormat("en-US", {
            style: "currency",
            currency: "USD"
        });
        const date = new Intl.DateTimeFormat("en-US", {
            year: "numeric",
            month: "short",
            day: "2-digit"
        });
        const list = new Intl.ListFormat("en-US", {
            type: "conjunction",
            style: "long"
        });
        const relative = new Intl.RelativeTimeFormat("en-US", {
            numeric: "always"
        });
        const duration = new Intl.DurationFormat("en-US", { style: "long" });

        const checks = [
            validateParts(number.formatToParts(12345.67), "number"),
            validateParts(number.formatRangeToParts(1, 2), "numberRange"),
            validateParts(date.formatToParts(0), "date"),
            validateParts(date.formatRangeToParts(0, 86400000), "dateRange"),
            validateParts(list.formatToParts(["alpha", "beta", "gamma"]), "list"),
            validateParts(relative.formatToParts(-2, "day"), "relative"),
            validateParts(
                duration.formatToParts({ hours: 1, minutes: 2, seconds: 3 }),
                "duration"
            )
        ];

        const options = [
            number.resolvedOptions(),
            date.resolvedOptions(),
            list.resolvedOptions(),
            relative.resolvedOptions(),
            duration.resolvedOptions()
        ];
        for (let i = 0; i < options.length; i++) {
            const held = options[i];
            churn(200 + i);
            if (held !== options[i] || typeof held.locale !== "string") {
                throw new Error("resolvedOptions roots " + i);
            }
        }

        checks.every(value => value > 0) && options.length === 5;
        "#,
        "<gc-intl-parts-builders>",
    );

    assert_eq!(completion, "true");
}

#[test]
fn locale_info_arrays_and_objects_survive_moving_gc() {
    let completion = run(
        r#"
        function churn(seed) {
            let tail = null;
            for (let i = 0; i < 24; i++) {
                tail = { seed, i, text: "locale-" + seed + "-" + i, tail };
            }
            return tail;
        }

        const locale = new Intl.Locale("en-US");
        const arrays = [
            locale.getCalendars(),
            locale.getCollations(),
            locale.getHourCycles(),
            locale.getNumberingSystems(),
            locale.getTimeZones()
        ];
        for (let i = 0; i < arrays.length; i++) {
            const held = arrays[i];
            churn(300 + i);
            if (!Array.isArray(held) || held.length === 0) {
                throw new Error("locale array roots " + i);
            }
            for (const value of held) {
                if (typeof value !== "string") {
                    throw new Error("locale string roots " + i);
                }
            }
        }

        const text = locale.getTextInfo();
        const week = locale.getWeekInfo();
        churn(400);
        if (
            typeof text.direction !== "string" ||
            typeof week.firstDay !== "number" ||
            !Array.isArray(week.weekend) ||
            week.weekend.join(",") !== "6,7"
        ) {
            throw new Error("locale info object roots");
        }

        arrays.length === 5 && week.weekend.length === 2;
        "#,
        "<gc-intl-locale-builders>",
    );

    assert_eq!(completion, "true");
}

#[test]
fn static_get_canonical_locales_keeps_its_result_rooted() {
    let completion = run(
        r#"
        const locales = ["en-US", "fr"];
        const result = Intl.getCanonicalLocales(locales);
        const first = Object.getOwnPropertyDescriptor(result, "0");
        const length = Object.getOwnPropertyDescriptor(result, "length");
        if (
            !Array.isArray(result) ||
            result.length !== 2 ||
            result[0] !== "en-US" ||
            result[1] !== "fr" ||
            first.value !== "en-US" ||
            first.writable !== true ||
            first.enumerable !== true ||
            first.configurable !== true ||
            length.value !== 2 ||
            length.writable !== true ||
            length.enumerable !== false ||
            length.configurable !== false
        ) {
            throw new Error("canonical locale array or descriptor roots");
        }
        result.length = 42;
        result.length === 42;
        "#,
        "<gc-intl-get-canonical-locales>",
    );

    assert_eq!(completion, "true");
}
