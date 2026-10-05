//! Architecture-independent, address-redacted return-site annotations.
//!
//! # Contents
//! - The exact installed table's code-relative machine returns and source ids.
//!
//! # Invariants
//! Both assembly renderers use this one formatter only under artifact capture.
//! Offsets belong to code.bin; no resolved address or inferred return is printed.
//!
//! # See also
//! - `otter_vm::native_abi::SafepointEntry` owns the association contract.

use otter_vm::native_abi::SafepointEntry;
use std::fmt::Write as _;

pub(super) fn render_return_site_summary(output: &mut String, sites: &[SafepointEntry]) {
    writeln!(output, "; js-return-sites={}", sites.len()).expect("writing to String cannot fail");
    for site in sites {
        writeln!(
            output,
            "; js-return +0x{:08x} safepoint={}",
            site.native_return_offset, site.safepoint_id
        )
        .expect("writing to String cannot fail");
    }
}

#[cfg(test)]
mod return_site_tests {
    #[test]
    fn exact_js_returns_name_existing_safepoints_without_inventing_record_offsets() {
        let mut output = String::new();
        super::render_return_site_summary(
            &mut output,
            &[
                otter_vm::native_abi::SafepointEntry {
                    native_return_offset: 4,
                    safepoint_id: 17,
                },
                otter_vm::native_abi::SafepointEntry {
                    native_return_offset: 28,
                    safepoint_id: 17,
                },
            ],
        );
        assert_eq!(
            output,
            "; js-return-sites=2\n; js-return +0x00000004 safepoint=17\n; js-return +0x0000001c safepoint=17\n"
        );
    }
}
