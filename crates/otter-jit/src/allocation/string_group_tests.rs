//! Executable complete-payload proofs for native strings and fixed groups.
//!
//! # Contents
//! - Flat width/hash, compressed Cons children, identities and pre-effect misses.
//! - Two/eight-shell groups with one publication and exact per-type accounting.
//! - Alternate declared GP banks and preserved live GP/FP canaries.
//!
//! # Invariants
//! These bounded host cells never reach a collector. The actual production
//! encoder executes with its native ABI, exact VM geometry and poisoned guards.
//! Every initialized byte and all 256 statistics rows are checked. Runtime
//! proofs separately own genuine moving cells and collecting group admission.
//!
//! # See also
//! - `crate::graph::allocation_groups` owns source membership and recovery.
//! - `otter-vm/src/runtime_stubs/string.rs` owns canonical primitive completion.

use super::*;
use crate::CompiledCode;
#[cfg(target_arch = "aarch64")]
use dynasmrt::aarch64::Assembler;
#[cfg(target_arch = "x86_64")]
use dynasmrt::x64::Assembler;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::{
    Value,
    jit::{JitEmptyArrayAllocationPlan, JitEmptyObjectAllocationPlan, JitStringLayout},
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
};
const POISON: u64 = 0xcafe_1357_2468_abcd;
const OLD_RESULT: u64 = 0x1357;
const CANARY: u64 = 0x51a7_6d9b_2468_1357;
enum Recipe {
    String(JitStringLayout),
    Group {
        realm: u32,
        members: Vec<(u32, EmptyLiteralLayout)>,
    },
}
fn run(
    recipe: Recipe,
    values: [u64; 3],
    expected: Vec<u64>,
    expected_counts: &[(u8, u64, usize)],
    regs: LabRegisters,
    fits: bool,
    allowed: bool,
    identity: Option<u64>,
    actual_realm: u32,
) {
    let case_label = format!("fit={fits} allowed={allowed} identity={identity:?} bank={regs:?}");
    let bytes = expected.len() * 8;
    let mut words = vec![POISON; expected.len() + 2];
    // SAFETY: complete aligned private host payload and guard words.
    let start = unsafe { words.as_mut_ptr().add(1) } as usize;
    let mut lab = LinearAllocationArea {
        top: start,
        limit: start + bytes - usize::from(!fits),
    };
    let mut stats = [TypeStats::DEFAULT; 256];
    let mut thread = VmThread::empty();
    thread.active_realm_cell = std::ptr::addr_of!(actual_realm) as u64;
    let mut ctx = JitCtx {
        thread: &mut thread,
        native_frame: std::ptr::null_mut(),
        error: std::ptr::null_mut(),
        generated_depth_limit: u64::MAX,
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow {
            lab: &mut lab,
            type_stats: stats.as_mut_ptr(),
        },
        runtime_stats: std::ptr::null_mut(),
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    };
    let mut canaries = [0u64; 64];
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    #[cfg(target_arch = "aarch64")]
    let gp = [
        1u8, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15, 19, 21, 22, 23, 24, 25, 26, 27, 28, 29,
    ];
    #[cfg(target_arch = "aarch64")]
    let fp = (0..31u8).collect::<Vec<_>>();
    #[cfg(target_arch = "x86_64")]
    let gp = [1u8, 5, 6, 7, 12, 13, 14];
    #[cfg(target_arch = "x86_64")]
    let fp = (0..15u8).collect::<Vec<_>>();
    assert!(
        ![
            regs.buffer,
            regs.candidate,
            regs.end,
            regs.scratch,
            regs.size
        ]
        .contains(&0)
    );
    assert!(gp.iter().all(|register| {
        ![
            regs.buffer,
            regs.candidate,
            regs.end,
            regs.scratch,
            regs.size,
        ]
        .contains(register)
    }));
    #[cfg(target_arch = "aarch64")]
    {
        dynasm!(ops ; .arch aarch64 ; stp x20,x30,[sp,#-16]! ; mov x20,x0 ; sub sp,sp,176 ; str x2,[sp,#24]);
        // Preserve the actual host ABI before installing every non-temporary
        // canary. x18 is platform-reserved and is never touched.
        let preserved_gp = [19u8, 21, 22, 23, 24, 25, 26, 27, 28, 29];
        for (index, &register) in preserved_gp.iter().enumerate() {
            let byte = 32 + index as u32 * 8;
            dynasm!(ops ; .arch aarch64 ; str X(register),[sp,byte]);
        }
        for register in 8..16u8 {
            let byte = 112 + u32::from(register - 8) * 8;
            dynasm!(ops ; .arch aarch64 ; str D(register),[sp,byte]);
        }
        for index in 0..3u32 {
            let byte = index * 8;
            dynasm!(ops ; .arch aarch64 ; ldr x16, [x1, byte] ; str x16, [sp, byte]);
        }
        for &register in &gp {
            crate::template::arm64::values::emit_load_u64(
                &mut ops,
                register,
                CANARY + u64::from(register),
            );
        }
        for &register in &fp {
            crate::template::arm64::values::emit_load_u64(
                &mut ops,
                16,
                CANARY + u64::from(register),
            );
            dynasm!(ops ; .arch aarch64 ; fmov D(register), x16);
        }
        crate::template::arm64::values::emit_load_u64(&mut ops, 0, OLD_RESULT);
        let input = [
            AllocationValue::StackByte(0),
            AllocationValue::StackByte(8),
            AllocationValue::StackByte(16),
        ];
        match &recipe {
            Recipe::String(layout) => crate::arm64::allocation::emit_concat(
                &mut ops,
                20,
                *layout,
                [input[0], input[1]],
                regs,
                slow,
            ),
            Recipe::Group { realm, members } => crate::arm64::allocation::emit_fixed_group(
                &mut ops,
                20,
                *realm,
                members,
                bytes as u32,
                regs,
                slow,
            ),
        }
        dynasm!(ops ; .arch aarch64 ; mov x0, X(regs.candidate) ; b =>done ; =>slow ; =>done ; ldr x16, [sp, #24]);
        for (index, &register) in gp.iter().enumerate() {
            let byte = index as u32 * 8;
            dynasm!(ops ; .arch aarch64 ; str X(register), [x16, byte]);
        }
        for &register in &fp {
            let byte = (gp.len() as u32 + u32::from(register)) * 8;
            dynasm!(ops ; .arch aarch64 ; str D(register), [x16, byte]);
        }
        for (index, &register) in preserved_gp.iter().enumerate() {
            let byte = 32 + index as u32 * 8;
            dynasm!(ops ; .arch aarch64 ; ldr X(register),[sp,byte]);
        }
        for register in 8..16u8 {
            let byte = 112 + u32::from(register - 8) * 8;
            dynasm!(ops ; .arch aarch64 ; ldr D(register),[sp,byte]);
        }
        dynasm!(ops ; .arch aarch64 ; add sp,sp,176 ; ldp x20,x30,[sp],#16 ; ret);
    }
    #[cfg(target_arch = "x86_64")]
    {
        dynasm!(ops ; .arch x64 ; push rbx ; push rbp ; push r12 ; push r13 ; push r14 ; push r15 ; mov r15, rdi ; sub rsp, 32 ; mov [rsp + 24], rdx);
        for index in 0..3i32 {
            let byte = index * 8;
            dynasm!(ops ; .arch x64 ; mov r10, [rsi + byte] ; mov [rsp + byte], r10);
        }
        for &register in &gp {
            crate::x86_64::values::emit_load_u64(&mut ops, register, CANARY + u64::from(register));
        }
        for &register in &fp {
            crate::x86_64::values::emit_load_u64(&mut ops, 10, CANARY + u64::from(register));
            dynasm!(ops ; .arch x64 ; movq Rx(register), r10);
        }
        crate::x86_64::values::emit_load_u64(&mut ops, 0, OLD_RESULT);
        let input = [
            AllocationValue::StackByte(0),
            AllocationValue::StackByte(8),
            AllocationValue::StackByte(16),
        ];
        match &recipe {
            Recipe::String(layout) => crate::x86_64::allocation::emit_concat(
                &mut ops,
                15,
                *layout,
                [input[0], input[1]],
                regs,
                slow,
            ),
            Recipe::Group { realm, members } => crate::x86_64::allocation::emit_fixed_group(
                &mut ops,
                15,
                *realm,
                members,
                bytes as u32,
                regs,
                slow,
            ),
        }
        dynasm!(ops ; .arch x64 ; mov rax, Rq(regs.candidate) ; jmp =>done ; =>slow ; =>done ; mov r10, [rsp + 24]);
        for (index, &register) in gp.iter().enumerate() {
            let byte = index as i32 * 8;
            dynasm!(ops ; .arch x64 ; mov [r10 + byte], Rq(register));
        }
        for &register in &fp {
            let byte = (gp.len() as i32 + i32::from(register)) * 8;
            dynasm!(ops ; .arch x64 ; movq [r10 + byte], Rx(register));
        }
        dynasm!(ops ; .arch x64 ; add rsp, 32 ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbp ; pop rbx ; ret);
    }
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the finalized fixture has this exact preserved native ABI.
    #[cfg(target_arch = "aarch64")]
    let call: extern "C" fn(*mut JitCtx, *const u64, *mut u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    // SAFETY: the finalized fixture has this exact private SysV ABI.
    #[cfg(target_arch = "x86_64")]
    let call: extern "sysv64" fn(*mut JitCtx, *const u64, *mut u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    // SAFETY: the fixture preserves its private ABI and uses complete bounded
    // host input/LAB/stat spans; no fabricated cell reaches the collector.
    let result = call(&mut ctx, values.as_ptr(), canaries.as_mut_ptr());
    let accepted = allowed && (fits || identity.is_some());
    let allocates = accepted && identity.is_none();
    assert_eq!(
        result,
        if accepted {
            identity.map_or(start, |bits| bits as usize)
        } else {
            OLD_RESULT as usize
        },
        "{case_label} result"
    );
    assert_eq!(words[0], POISON, "{case_label} preceding guard");
    assert_eq!(
        *words.last().unwrap(),
        POISON,
        "{case_label} trailing guard"
    );
    for (index, &register) in gp.iter().enumerate() {
        assert_eq!(
            canaries[index],
            CANARY + u64::from(register),
            "{case_label} GP{register}"
        );
    }
    for (register, &physical) in fp.iter().enumerate() {
        assert_eq!(
            canaries[gp.len() + register],
            CANARY + u64::from(physical),
            "{case_label} FP{physical}"
        );
    }
    if allocates {
        assert_eq!(lab.top, start + bytes, "{case_label} publication");
        assert_eq!(
            &words[1..words.len() - 1],
            expected,
            "{case_label} complete payload"
        );
    } else {
        assert_eq!(lab.top, start, "{case_label} no publication");
        assert!(
            words.iter().all(|word| *word == POISON),
            "{case_label} no partial initialization"
        );
    }
    for (index, row) in stats.iter().enumerate() {
        let (count, owned_bytes) = if allocates {
            expected_counts
                .iter()
                .find(|(tag, _, _)| usize::from(*tag) == index)
                .map(|(_, count, bytes)| (*count, *bytes))
                .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        assert_eq!(
            row.alloc_count_total, count,
            "{case_label} tag{index} count"
        );
        assert_eq!(
            row.alloc_bytes_total, owned_bytes as u64,
            "{case_label} tag{index} bytes"
        );
        assert_eq!(row.live_bytes, owned_bytes, "{case_label} tag{index} live");
        assert_eq!(row.free_count_total, 0, "{case_label} tag{index} free");
    }
}

fn banks() -> [LabRegisters; 2] {
    #[cfg(target_arch = "aarch64")]
    {
        [
            LabRegisters {
                buffer: 6,
                candidate: 7,
                end: 8,
                scratch: 9,
                size: 17,
            },
            LabRegisters {
                buffer: 8,
                candidate: 9,
                end: 6,
                scratch: 7,
                size: 17,
            },
        ]
    }
    #[cfg(target_arch = "x86_64")]
    {
        [
            LabRegisters {
                buffer: 2,
                candidate: 3,
                end: 8,
                scratch: 9,
                size: 11,
            },
            LabRegisters {
                buffer: 8,
                candidate: 9,
                end: 2,
                scratch: 3,
                size: 11,
            },
        ]
    }
}
fn bytes_mut(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: this exclusive bounded byte view covers exactly initialized words.
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast(), words.len() * 8) }
}
fn put32(words: &mut [u64], byte: u32, value: u32) {
    bytes_mut(words)[byte as usize..byte as usize + 4].copy_from_slice(&value.to_le_bytes());
}
fn hash(layout: JitStringLayout, units: &[u16]) -> u64 {
    units
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .fold(layout.fnv_offset, |value, byte| {
            (value ^ u64::from(byte)).wrapping_mul(layout.fnv_prime)
        })
}
fn flat(layout: JitStringLayout, units: &[u16], latin: bool, seq: bool) -> Vec<u64> {
    let payload = if seq {
        layout.string_body_size
    } else {
        layout.string_repr_payload_byte
    };
    let bytes = layout
        .cell_bytes
        .max(payload + units.len() as u32 * if latin { 1 } else { 2 });
    let mut words = vec![0; (bytes as usize).div_ceil(8)];
    words[0] = layout.header_word;
    put32(&mut words, layout.string_len_byte, units.len() as u32);
    words[layout.hash_byte as usize / 8] = hash(layout, units);
    bytes_mut(&mut words)[layout.string_repr_byte as usize] = match (latin, seq) {
        (true, false) => layout.inline_latin1_tag,
        (true, true) => layout.seq_latin1_tag,
        (false, false) => layout.inline_flat_tag,
        (false, true) => layout.seq_flat_tag,
    };
    for (index, &unit) in units.iter().enumerate() {
        if latin {
            bytes_mut(&mut words)[payload as usize + index] = unit as u8;
        } else {
            bytes_mut(&mut words)[payload as usize + index * 2..payload as usize + index * 2 + 2]
                .copy_from_slice(&unit.to_le_bytes());
        }
    }
    words
}
#[test]
fn strings_execute_exact_flat_cons_identity_and_pre_effect_refusal_payloads() {
    let p = JitStringLayout::default();
    for regs in banks() {
        for (left, right, left_latin, right_latin, seq) in [
            (vec![97, 98], vec![99, 100], true, true, false),
            (vec![65], vec![0x100, 0xd800, 0xdc01], true, false, false),
            (vec![0x100, 0xd800], vec![66], false, true, false),
            (vec![97; 25], vec![98], true, true, true),
        ] {
            let a = flat(p, &left, left_latin, seq);
            let b = flat(p, &right, right_latin, false);
            let values = [
                a.as_ptr() as u64,
                b.as_ptr() as u64,
                Value::UNDEFINED.to_bits(),
            ];
            let combined = [left.clone(), right.clone()].concat();
            let latin = left_latin && right_latin;
            let mut expected = if combined.len()
                <= (if latin {
                    p.inline_latin1_cap
                } else {
                    p.inline_flat_cap
                }) as usize
            {
                flat(p, &combined, latin, false)
            } else {
                let mut v = vec![0; p.cell_bytes as usize / 8];
                v[0] = p.header_word;
                put32(&mut v, p.string_len_byte, combined.len() as u32);
                put32(&mut v, p.cons_left_byte, values[0] as u32);
                put32(&mut v, p.cons_right_byte, values[1] as u32);
                v[p.hash_byte as usize / 8] = a[p.hash_byte as usize / 8].wrapping_mul(p.fnv_prime)
                    ^ b[p.hash_byte as usize / 8];
                bytes_mut(&mut v)[p.string_repr_byte as usize] = p.cons_tag;
                bytes_mut(&mut v)[p.cons_depth_byte as usize] = 1;
                v
            };
            expected.resize(p.cell_bytes as usize / 8, 0);
            for fits in [true, false] {
                run(
                    Recipe::String(p),
                    values,
                    expected.clone(),
                    &[(p.string_type_tag, 1, p.cell_bytes as usize)],
                    regs,
                    fits,
                    true,
                    None,
                    0,
                );
            }
        }
        let empty = flat(p, &[], true, false);
        let nonempty = flat(p, &[0x100], false, false);
        for reverse in [false, true] {
            let values = if reverse {
                [nonempty.as_ptr() as u64, empty.as_ptr() as u64, 0]
            } else {
                [empty.as_ptr() as u64, nonempty.as_ptr() as u64, 0]
            };
            run(
                Recipe::String(p),
                values,
                vec![0; p.cell_bytes as usize / 8],
                &[],
                regs,
                false,
                true,
                Some(nonempty.as_ptr() as u64),
                0,
            );
        }
        let good = flat(p, &[97], true, false);
        let values = [Value::boolean(true).to_bits(), good.as_ptr() as u64, 0];
        run(
            Recipe::String(p),
            values,
            vec![0; p.cell_bytes as usize / 8],
            &[],
            regs,
            true,
            false,
            None,
            0,
        );
        let mut too_long = good.clone();
        put32(&mut too_long, p.string_len_byte, u32::MAX);
        run(
            Recipe::String(p),
            [too_long.as_ptr() as u64, good.as_ptr() as u64, 0],
            vec![0; p.cell_bytes as usize / 8],
            &[],
            regs,
            true,
            false,
            None,
            0,
        );
        let mut deep = good.clone();
        bytes_mut(&mut deep)[p.string_repr_byte as usize] = p.cons_tag;
        bytes_mut(&mut deep)[p.cons_depth_byte as usize] = p.max_rope_depth as u8;
        run(
            Recipe::String(p),
            [deep.as_ptr() as u64, good.as_ptr() as u64, 0],
            vec![0; p.cell_bytes as usize / 8],
            &[],
            regs,
            true,
            false,
            None,
            0,
        );
    }
}
#[test]
fn fixed_groups_initialize_every_cell_before_one_publication_and_account_once() {
    for regs in banks() {
        for count in [2usize, 8] {
            let object = EmptyLiteralLayout::Object(JitEmptyObjectAllocationPlan::new(4096));
            let array = EmptyLiteralLayout::Array(JitEmptyArrayAllocationPlan::default());
            let mut expected = Vec::new();
            let mut members = Vec::new();
            let mut counts = Vec::<(u8, u64, usize)>::new();
            for index in 0..count {
                let layout = if index % 2 == 0 { object } else { array };
                members.push((expected.len() as u32 * 8, layout));
                let mut cell = vec![0; layout.bytes() as usize / 8];
                cell[0] = layout.header();
                if let EmptyLiteralLayout::Object(p) = layout {
                    cell[p.shape_byte as usize / 8] = u64::from(p.shape);
                    for byte in p.initial_value_bytes {
                        cell[byte as usize / 8] = Value::UNDEFINED.to_bits();
                    }
                }
                expected.extend(cell);
                if let Some(row) = counts.iter_mut().find(|r| r.0 == layout.header() as u8) {
                    row.1 += 1;
                    row.2 += layout.bytes() as usize;
                } else {
                    counts.push((layout.header() as u8, 1, layout.bytes() as usize));
                }
            }
            for (fits, realm) in [(true, 0), (false, 0), (true, 1)] {
                run(
                    Recipe::Group {
                        realm: 0,
                        members: members.clone(),
                    },
                    [0; 3],
                    expected.clone(),
                    &counts,
                    regs,
                    fits,
                    realm == 0,
                    None,
                    realm,
                );
            }
            run(
                Recipe::Group { realm: 1, members },
                [0; 3],
                expected,
                &counts,
                regs,
                true,
                false,
                None,
                1,
            );
        }
    }
}
