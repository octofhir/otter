use crate::compiler::CompilerBackend;
use crate::{JitCompileArtifact, JitError};
use cranelift_codegen::isa::CallConv;
use dynasmrt::{DynasmApi, dynasm, DynasmLabelApi};
use otter_vm_bytecode::Function;
use otter_vm_bytecode::operand::Register;
use crate::BailoutReason;

unsafe extern "C" {
    pub fn baseline_get_local(ctx: *mut u8, idx: u32) -> i64;
    pub fn baseline_set_local(ctx: *mut u8, idx: u32, val_raw: i64);
    pub fn baseline_is_truthy(val_raw: i64) -> u32;
    pub fn baseline_get_prop_const(
        ctx: *mut u8,
        function_ptr: *const otter_vm_bytecode::Function,
        obj_raw: i64,
        const_idx: u32,
        ic_index: u32,
    ) -> i64;
    pub fn baseline_set_prop_const(
        ctx: *mut u8,
        function_ptr: *const otter_vm_bytecode::Function,
        obj_raw: i64,
        const_idx: u32,
        val_raw: i64,
        ic_index: u32,
    );
    pub fn baseline_call(
        ctx: *mut u8,
        function_ptr: *const otter_vm_bytecode::Function,
        callee_raw: i64,
        argc: u32,
        ic_index: u32,
    ) -> i64;
    pub fn baseline_call_method(
        ctx: *mut u8,
        function_ptr: *const otter_vm_bytecode::Function,
        obj_raw: i64,
        const_idx: u32,
        argc: u32,
        ic_index: u32,
    ) -> i64;
    pub fn baseline_arith_add(ctx: *mut u8, lhs: i64, rhs: i64, ic_idx: u32) -> i64;
    pub fn baseline_arith_sub(ctx: *mut u8, lhs: i64, rhs: i64, ic_idx: u32) -> i64;
    pub fn baseline_arith_mul(ctx: *mut u8, lhs: i64, rhs: i64, ic_idx: u32) -> i64;
    pub fn baseline_arith_div(ctx: *mut u8, lhs: i64, rhs: i64, ic_idx: u32) -> i64;
}

fn load_u64_into_x8(ops: &mut dynasmrt::aarch64::Assembler, value: u64) {
    let b0 = (value & 0xFFFF) as u32;
    let b1 = ((value >> 16) & 0xFFFF) as u32;
    let b2 = ((value >> 32) & 0xFFFF) as u32;
    let b3 = ((value >> 48) & 0xFFFF) as u32;
    dynasm!(ops
        ; movz x8, #b0
        ; movk x8, #b1, lsl 16
        ; movk x8, #b2, lsl 32
        ; movk x8, #b3, lsl 48
    );
}

fn load_u64_into_x1(ops: &mut dynasmrt::aarch64::Assembler, value: u64) {
    let b0 = (value & 0xFFFF) as u32;
    let b1 = ((value >> 16) & 0xFFFF) as u32;
    let b2 = ((value >> 32) & 0xFFFF) as u32;
    let b3 = ((value >> 48) & 0xFFFF) as u32;
    dynasm!(ops
        ; movz x1, #b0
        ; movk x1, #b1, lsl 16
        ; movk x1, #b2, lsl 32
        ; movk x1, #b3, lsl 48
    );
}

pub struct BaselineCompiler {}

impl BaselineCompiler {
    pub fn new() -> Self {
        Self {}
    }
}

fn reg_offset(reg: Register) -> i32 {
    let index = reg.index() as i32;
    -16 - (index * 8)
}

impl CompilerBackend for BaselineCompiler {
    fn compile_function(&mut self, function: &Function) -> Result<JitCompileArtifact, JitError> {
        let instructions = function.instructions.read();

        #[cfg(target_arch = "x86_64")]
        let code = {
            let mut ops =
                dynasmrt::x64::Assembler::new().map_err(|e| JitError::Builder(e.to_string()))?;

            dynasm!(ops
                ; .arch x64
                ; push rbp
                ; mov rbp, rsp
                ; sub rsp, (function.local_count as i32 * 8)
            );

            for (_pc, instr) in instructions.iter().enumerate() {
                match instr {
                    otter_vm_bytecode::Instruction::LoadUndefined { dst } => {
                        let undefined_bits = 0x7FF8_0000_0000_0000u64 as i64;
                        let offset = reg_offset(*dst);
                        dynasm!(ops
                            ; mov rax, QWORD undefined_bits
                            ; mov [rbp + offset], rax
                        );
                    }
                    otter_vm_bytecode::Instruction::LoadInt32 { dst, value } => {
                        let int32_bits = (0x7FF0_0000_0000_0000u64 | (*value as u32 as u64)) as i64;
                        let offset = reg_offset(*dst);
                        dynasm!(ops
                            ; mov rax, QWORD int32_bits
                            ; mov [rbp + offset], rax
                        );
                    }
                    otter_vm_bytecode::Instruction::Add { dst, lhs, rhs, .. } => {
                        let lhs_off = reg_offset(*lhs);
                        let rhs_off = reg_offset(*rhs);
                        let dst_off = reg_offset(*dst);
                        dynasm!(ops
                            ; mov rax, [rbp + lhs_off]
                            ; add rax, [rbp + rhs_off]
                            ; mov [rbp + dst_off], rax
                        );
                    }
                    otter_vm_bytecode::Instruction::Return { src } => {
                        let src_off = reg_offset(*src);
                        dynasm!(ops
                            ; mov rax, [rbp + src_off]
                            ; mov rsp, rbp
                            ; pop rbp
                            ; ret
                        );
                    }
                    otter_vm_bytecode::Instruction::GetPropConst { .. } => {}
                    _ => {}
                }
            }

            // Epilogue fallback
            dynasm!(ops
                ; mov rsp, rbp
                ; pop rbp
                ; ret
            );
            ops.finalize().unwrap()
        };

        #[cfg(target_arch = "aarch64")]
        let code = {
            // Pre-scan for unsupported instructions
            for instr in instructions.iter() {
                match instr {
                    otter_vm_bytecode::Instruction::LoadUndefined { .. }
                    | otter_vm_bytecode::Instruction::LoadInt32 { .. }
                    | otter_vm_bytecode::Instruction::Add { .. }
                    | otter_vm_bytecode::Instruction::GetLocal { .. }
                    | otter_vm_bytecode::Instruction::SetLocal { .. }
                    | otter_vm_bytecode::Instruction::Move { .. }
                    | otter_vm_bytecode::Instruction::Return { .. }
                    | otter_vm_bytecode::Instruction::Jump { .. }
                    | otter_vm_bytecode::Instruction::JumpIfTrue { .. }
                    | otter_vm_bytecode::Instruction::JumpIfFalse { .. }
                    | otter_vm_bytecode::Instruction::GetPropConst { .. }
                    | otter_vm_bytecode::Instruction::SetPropConst { .. }
                    | otter_vm_bytecode::Instruction::Sub { .. }
                    | otter_vm_bytecode::Instruction::Mul { .. }
                    | otter_vm_bytecode::Instruction::Div { .. }
                    | otter_vm_bytecode::Instruction::AddInt32 { .. }
                    | otter_vm_bytecode::Instruction::SubInt32 { .. }
                    | otter_vm_bytecode::Instruction::MulInt32 { .. }
                    | otter_vm_bytecode::Instruction::DivInt32 { .. } => {}
                    _ => return Err(JitError::Builder(format!("Unsupported instruction: {:?}", instr))),
                }
            }

            let mut ops = dynasmrt::aarch64::Assembler::new()
                .map_err(|e| JitError::Builder(e.to_string()))?;

            let labels: Vec<_> = (0..instructions.len())
                .map(|_| ops.new_dynamic_label())
                .collect();

            // Prologue: save frame pointer, link register, and callee-saved x20, x21
            dynasm!(ops
                ; .arch aarch64
                ; stp x29, x30, [sp, -16]! // push fp, lr & update sp
                ; mov x29, sp              // set new frame pointer
                ; stp x20, x21, [sp, -16]! // push x20, x21
                ; mov x20, x0              // pin VmContext inside x20
                ; mov x21, x2              // pin result_out pointer inside x21
            );

            // Align local space to 16 bytes
            let local_count = function.local_count as i32;
            let local_bytes = (local_count * 8 + 15) & !15;
            let local_u32 = local_bytes as u32;
            if local_bytes > 0 {
                dynasm!(ops
                    ; sub sp, sp, #local_u32
                );

                // Copy arguments from x1 (args_ptr) to stack locals
                // x3 contains argc
                let param_count = function.param_count as u32;
                if param_count > 0 {
                    // Pre-zero locals? No, interpreter doesn't necessarily do that for all.
                    // But we should copy up to min(argc, param_count)
                    for i in 0..param_count {
                        let offset = -16 - (i as i32 * 8);
                        let arg_idx_bytes = i * 8;
                        dynasm!(ops
                            ; cmp w3, #i
                            ; b.ls >skip_arg
                            ; ldr x8, [x1, #arg_idx_bytes]
                            ; stur x8, [x29, #offset]
                            ; skip_arg:
                        );
                    }
                }
            }

            for (pc, instr) in instructions.iter().enumerate() {
                let lbl = labels[pc];
                dynasm!(ops
                    ; =>lbl
                );
                match instr {
                    otter_vm_bytecode::Instruction::LoadUndefined { dst } => {
                        let undefined_bits = 0x7FF8_0000_0000_0000u64;
                        let offset = reg_offset(*dst);

                        // Load 64-bit constant using movz/movk
                        let b0 = (undefined_bits & 0xFFFF) as u32;
                        let b1 = ((undefined_bits >> 16) & 0xFFFF) as u32;
                        let b2 = ((undefined_bits >> 32) & 0xFFFF) as u32;
                        let b3 = ((undefined_bits >> 48) & 0xFFFF) as u32;

                        dynasm!(ops
                            ; movz x8, #b0
                            ; movk x8, #b1, lsl 16
                            ; movk x8, #b2, lsl 32
                            ; movk x8, #b3, lsl 48
                            ; stur x8, [x29, #offset]
                        );
                    }
                    otter_vm_bytecode::Instruction::LoadInt32 { dst, value } => {
                        let int32_bits = 0x7FF0_0000_0000_0000u64 | (*value as u32 as u64);
                        let offset = reg_offset(*dst);

                        let b0 = (int32_bits & 0xFFFF) as u32;
                        let b1 = ((int32_bits >> 16) & 0xFFFF) as u32;
                        let b2 = ((int32_bits >> 32) & 0xFFFF) as u32;
                        let b3 = ((int32_bits >> 48) & 0xFFFF) as u32;

                        dynasm!(ops
                            ; movz x8, #b0
                            ; movk x8, #b1, lsl 16
                            ; movk x8, #b2, lsl 32
                            ; movk x8, #b3, lsl 48
                            ; stur x8, [x29, #offset]
                        );
                    }
                    otter_vm_bytecode::Instruction::Add { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::Sub { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::Mul { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::Div { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::AddInt32 { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::SubInt32 { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::MulInt32 { dst, lhs, rhs, feedback_index }
                    | otter_vm_bytecode::Instruction::DivInt32 { dst, lhs, rhs, feedback_index } => {
                        let lhs_off = reg_offset(*lhs);
                        let rhs_off = reg_offset(*rhs);
                        let dst_off = reg_offset(*dst);
                        let ic_idx = *feedback_index as u32;

                        let is_add = matches!(instr, otter_vm_bytecode::Instruction::Add { .. } | otter_vm_bytecode::Instruction::AddInt32 { .. });
                        let is_sub = matches!(instr, otter_vm_bytecode::Instruction::Sub { .. } | otter_vm_bytecode::Instruction::SubInt32 { .. });
                        let is_mul = matches!(instr, otter_vm_bytecode::Instruction::Mul { .. } | otter_vm_bytecode::Instruction::MulInt32 { .. });
                        let _is_div = matches!(instr, otter_vm_bytecode::Instruction::Div { .. } | otter_vm_bytecode::Instruction::DivInt32 { .. });

                        let tag_int32: u64 = 0x7FF8_0001_0000_0000;
                        let tag_mask: u64 = 0xFFFF_FFFF_0000_0000;

                        // x10 = ctx (JitContext*)
                        // x11 = lhs, x12 = rhs
                        dynasm!(ops
                            ; ldur x11, [x29, #lhs_off]
                            ; ldur x12, [x29, #rhs_off]
                        );

                        // Check if both are Int32
                        // x13 = tag_mask, x14 = tag_int32
                        let b0 = (tag_mask & 0xFFFF) as u32;
                        let b1 = ((tag_mask >> 16) & 0xFFFF) as u32;
                        let b2 = ((tag_mask >> 32) & 0xFFFF) as u32;
                        let b3 = ((tag_mask >> 48) & 0xFFFF) as u32;
                        dynasm!(ops
                            ; movz x13, #b0
                            ; movk x13, #b1, lsl 16
                            ; movk x13, #b2, lsl 32
                            ; movk x13, #b3, lsl 48
                        );
                        let b0 = (tag_int32 & 0xFFFF) as u32;
                        let b1 = ((tag_int32 >> 16) & 0xFFFF) as u32;
                        let b2 = ((tag_int32 >> 32) & 0xFFFF) as u32;
                        let b3 = ((tag_int32 >> 48) & 0xFFFF) as u32;
                        dynasm!(ops
                            ; movz x14, #b0
                            ; movk x14, #b1, lsl 16
                            ; movk x14, #b2, lsl 32
                            ; movk x14, #b3, lsl 48
                        );

                        dynasm!(ops
                            ; and x15, x11, x13
                            ; cmp x15, x14
                            ; b.ne >fallback
                            ; and x15, x12, x13
                            ; cmp x15, x14
                            ; b.ne >fallback
                        );

                        // Both are Int32. w11 = lhs value, w12 = rhs value (32-bit truncated)
                        if is_add {
                            dynasm!(ops
                                ; adds w15, w11, w12
                                ; b.vs >fallback
                            );
                        } else if is_sub {
                            dynasm!(ops
                                ; subs w15, w11, w12
                                ; b.vs >fallback
                            );
                        } else if is_mul {
                            // Multiplication overflow is more complex in 32-bit.
                            // Smull calculates 32*32 -> 64.
                            dynasm!(ops
                                ; smull x15, w11, w12
                                ; asr x16, x15, #31
                                ; cmp w16, w15, asr #31
                                ; b.ne >fallback
                            );
                        } else {
                            // Division: always fallback for now because of 0 check and float result
                            dynasm!(ops ; b >fallback);
                        }

                        // Success: Box result. x15 = int32 value
                        // Tag is in x14
                        dynasm!(ops
                            ; mov w15, w15
                            ; orr x15, x15, x14
                            ; stur x15, [x29, #dst_off]
                            ; b >done
                        );

                        dynasm!(ops
                            ; fallback:
                            ; mov x0, x20            // x0 = ctx (JitContext*)
                            ; mov x1, x11            // x1 = lhs
                            ; mov x2, x12            // x1 = rhs
                            ; movz w3, #ic_idx       // w3 = ic_idx
                        );
                        let helper_addr = if is_add {
                            baseline_arith_add
                        } else if is_sub {
                            baseline_arith_sub
                        } else if is_mul {
                            baseline_arith_mul
                        } else {
                            baseline_arith_div
                        } as *const u8 as u64;

                        load_u64_into_x8(&mut ops, helper_addr);
                        dynasm!(ops
                            ; blr x8
                            ; stur x0, [x29, #dst_off]
                            ; done:
                        );
                    }
                    otter_vm_bytecode::Instruction::GetLocal { dst, idx } => {
                        let dst_off = reg_offset(*dst);
                        let src_off = -16 - (idx.0 as i32 * 8);
                        dynasm!(ops
                            ; ldur x8, [x29, #src_off]
                            ; stur x8, [x29, #dst_off]
                        );
                    }
                    otter_vm_bytecode::Instruction::SetLocal { src, idx } => {
                        let src_off = reg_offset(*src);
                        let dst_off = -16 - (idx.0 as i32 * 8);
                        dynasm!(ops
                            ; ldur x8, [x29, #src_off]
                            ; stur x8, [x29, #dst_off]
                        );
                    }
                    otter_vm_bytecode::Instruction::Move { dst, src } => {
                        let dst_off = reg_offset(*dst);
                        let src_off = reg_offset(*src);
                        dynasm!(ops
                            ; ldur x8, [x29, #src_off]
                            ; stur x8, [x29, #dst_off]
                        );
                    }
                    otter_vm_bytecode::Instruction::Return { src } => {
                        let src_off = reg_offset(*src);
                        dynasm!(ops
                            ; ldur x0, [x29, #src_off] // return value in x0
                            ; stur x0, [x21]           // write out the return value into result_out
                            ; mov w0, #0               // mark JIT compilation success status (0)
                            ; ldp x20, x21, [sp], 16   // pop callee-saved
                            ; mov sp, x29              // restore sp
                            ; ldp x29, x30, [sp], 16   // pop fp, lr
                            ; ret
                        );
                    }
                    otter_vm_bytecode::Instruction::Jump { offset } => {
                        let target_pc = (pc as isize + offset.0 as isize) as usize;
                        let target_lbl = labels[target_pc];
                        dynasm!(ops
                            ; b =>target_lbl
                        );
                    }
                    otter_vm_bytecode::Instruction::JumpIfTrue { cond, offset } => {
                        let cond_off = reg_offset(*cond);
                        let target_pc = (pc as isize + offset.0 as isize) as usize;
                        let target_lbl = labels[target_pc];
                        let func_ptr = baseline_is_truthy as *const u8 as u64;
                        load_u64_into_x8(&mut ops, func_ptr);
                        dynasm!(ops
                            ; ldur x0, [x29, #cond_off]
                            ; blr x8
                            ; cmp w0, #0
                            ; b.ne =>target_lbl
                        );
                    }
                    otter_vm_bytecode::Instruction::JumpIfFalse { cond, offset } => {
                        let cond_off = reg_offset(*cond);
                        let target_pc = (pc as isize + offset.0 as isize) as usize;
                        let target_lbl = labels[target_pc];
                        let func_ptr = baseline_is_truthy as *const u8 as u64;
                        load_u64_into_x8(&mut ops, func_ptr);
                        dynasm!(ops
                            ; ldur x0, [x29, #cond_off]
                            ; blr x8
                            ; cmp w0, #0
                            ; b.eq =>target_lbl
                        );
                    }
                    otter_vm_bytecode::Instruction::Call { dst, func, argc, ic_index } => {
                        let dst_off = reg_offset(*dst);
                        let func_off = reg_offset(*func);
                        let ic_idx_val = *ic_index as u32;
                        let argc_val = *argc as u32;
                        
                        let helper_ptr = baseline_call as *const u8 as u64;
                        load_u64_into_x8(&mut ops, helper_ptr);
                        
                        let function_addr = function as *const Function as u64;
                        load_u64_into_x1(&mut ops, function_addr);

                        dynasm!(ops
                            ; mov x0, x20              // x0 = ctx
                            // x1 = function_ptr (set above)
                            ; ldur x2, [x29, #func_off] // x2 = callee_raw
                            ; movz w3, #argc_val       // w3 = argc
                            ; movz w4, #ic_idx_val     // w4 = ic_index
                            ; blr x8                   // returns callee value in x0
                            
                            // For now, we still need to actually perform the call.
                            // The helper just updated the IC and returned the callee.
                            // In a real JIT, we'd now check if it's a native function and call it,
                            // or if it's a closure and enter it.
                            
                            // To keep it simple for Phase 1, we'll just bailout after the IC update
                            // so the interpreter can handle the actual call dispatch logic.
                            ; mov w0, #BailoutReason::ComplexOperation as u32
                            ; b ->bailout
                        );
                    }
                    otter_vm_bytecode::Instruction::CallMethod { dst, obj, method, argc, ic_index } => {
                        let dst_off = reg_offset(*dst);
                        let obj_off = reg_offset(*obj);
                        let const_idx = method.0;
                        let ic_idx_val = *ic_index as u32;
                        let argc_val = *argc as u32;

                        let helper_ptr = baseline_call_method as *const u8 as u64;
                        load_u64_into_x8(&mut ops, helper_ptr);
                        
                        let function_addr = function as *const Function as u64;
                        load_u64_into_x1(&mut ops, function_addr);

                        dynasm!(ops
                            ; mov x0, x20              // x0 = ctx
                            // x1 = function_ptr (set above)
                            ; ldur x2, [x29, #obj_off]  // x2 = obj_raw
                            ; movz w3, #const_idx      // w3 = const_idx
                            ; movz w4, #argc_val       // w4 = argc
                            ; movz w5, #ic_idx_val     // w5 = ic_index
                            ; blr x8                   // returns method value in x0
                            
                            // Same as Call, bailout for now to let interpreter dispatch.
                            ; mov w0, #BailoutReason::ComplexOperation as u32
                            ; b ->bailout
                        );
                    }
                    otter_vm_bytecode::Instruction::GetPropConst { dst, obj, name, ic_index } => {
                        let dst_off = reg_offset(*dst);
                        let obj_off = reg_offset(*obj);
                        let const_idx = name.0 as u32;
                        let ic_idx_val = *ic_index as u32;
                        let func_ptr = baseline_get_prop_const as *const u8 as u64;
                        let function_addr = function as *const Function as u64;
                        load_u64_into_x8(&mut ops, func_ptr);
                        load_u64_into_x1(&mut ops, function_addr); // x1 = function_ptr
                        dynasm!(ops
                            ; mov x0, x20            // x0 = ctx
                            // x1 is already set above
                            ; ldur x2, [x29, #obj_off] // x2 = obj_raw
                            ; movz w3, #const_idx    // w3 = const_idx
                            ; movz w4, #ic_idx_val   // w4 = ic_index
                            ; blr x8
                            ; stur x0, [x29, #dst_off] // store result
                        );
                    }
                    otter_vm_bytecode::Instruction::SetPropConst { obj, name, val, ic_index } => {
                        let obj_off = reg_offset(*obj);
                        let val_off = reg_offset(*val);
                        let const_idx = name.0 as u32;
                        let ic_idx_val = *ic_index as u32;
                        let func_ptr = baseline_set_prop_const as *const u8 as u64;
                        let function_addr = function as *const Function as u64;
                        load_u64_into_x8(&mut ops, func_ptr);
                        load_u64_into_x1(&mut ops, function_addr); // x1 = function_ptr
                        dynasm!(ops
                            ; mov x0, x20              // x0 = ctx
                            // x1 is already set above
                            ; ldur x2, [x29, #obj_off] // x2 = obj_raw
                            ; movz w3, #const_idx      // w3 = const_idx
                            ; ldur x4, [x29, #val_off] // x4 = val_raw
                            ; movz w5, #ic_idx_val     // w5 = ic_index
                            ; blr x8
                        );
                    }
                    _ => {}
                }
            }

            // Epilogue fallback
            dynasm!(ops
                ; ldp x20, x21, [sp], 16
                ; mov sp, x29
                ; ldp x29, x30, [sp], 16
                ; ret
            );
            ops.finalize().unwrap()
        };

        let ptr = code.ptr(dynasmrt::AssemblyOffset(0));
        let len = code.len() as u64;

        Ok(JitCompileArtifact {
            code_ptr: ptr,
            code_size_bytes: len,
            _owned_code: Some(crate::compiler::OwnedJitCode::new_dynasm(code)),
        })
    }

    fn host_call_conv(&self) -> CallConv {
        #[cfg(target_os = "macos")]
        {
            #[cfg(target_arch = "aarch64")]
            return CallConv::AppleAarch64;
            #[cfg(target_arch = "x86_64")]
            return CallConv::SystemV;
        }
        #[cfg(not(target_os = "macos"))]
        CallConv::SystemV
    }
}
