// SPDX-License-Identifier: MIT OR Apache-2.0
//! Formatting utilities for VM trace and debug output.
//!
//! Provides human-readable formatting for snapshots, call stacks, and trace buffers.

use crate::context::VmContextSnapshot;
use crate::trace::{TraceEntry, TraceRingBuffer};
use std::fmt::Write;

/// Format a VM context snapshot for human-readable output
pub fn format_snapshot(snapshot: &VmContextSnapshot, trace_buffer: Option<&TraceRingBuffer>) -> String {
    let mut output = String::new();

    // VM State Section
    writeln!(&mut output, "VM State Snapshot:").unwrap();
    writeln!(&mut output, "  Stack Depth: {}", snapshot.stack_depth).unwrap();
    writeln!(&mut output, "  Native Depth: {}", snapshot.native_call_depth).unwrap();

    if let Some(current_module) = &snapshot.module_url {
        writeln!(&mut output, "  Current Module: {}", current_module).unwrap();
    }

    writeln!(&mut output).unwrap();

    // Current Frame Section
    if let Some(current_frame) = &snapshot.current_frame {
        writeln!(&mut output, "Current Frame:").unwrap();

        if let Some(name) = &current_frame.function_name {
            writeln!(&mut output, "  Function: {} (index: {})", name, current_frame.function_index).unwrap();
        } else {
            writeln!(&mut output, "  Function: <anonymous> (index: {})", current_frame.function_index).unwrap();
        }

        writeln!(&mut output, "  Module: {}", current_frame.module_url).unwrap();

        writeln!(&mut output, "  PC: {:#06x}", current_frame.pc).unwrap();

        if let Some(inst) = &current_frame.instruction {
            writeln!(&mut output, "  Instruction: {}", inst).unwrap();
        }

        writeln!(&mut output).unwrap();
    }

    // Call Stack Section
    if !snapshot.call_stack.is_empty() {
        writeln!(&mut output, "Call Stack ({} frames):", snapshot.call_stack.len()).unwrap();
        write!(&mut output, "{}", format_call_stack(&snapshot.call_stack)).unwrap();
        writeln!(&mut output).unwrap();
    }

    // Recent Instructions Section
    if let Some(buffer) = trace_buffer {
        if !buffer.is_empty() {
            writeln!(&mut output, "Recent Instructions (last {}):", buffer.len()).unwrap();
            write!(&mut output, "{}", format_trace_buffer(buffer)).unwrap();
            writeln!(&mut output).unwrap();
        }
    }

    output
}

/// Format call stack with detailed frame information
pub fn format_call_stack(frames: &[crate::context::FrameSnapshot]) -> String {
    let mut output = String::new();

    for (i, frame) in frames.iter().enumerate() {
        // Frame header
        write!(&mut output, "  #{:<3} ", i).unwrap();

        if let Some(name) = &frame.function_name {
            write!(&mut output, "{}", name).unwrap();
        } else {
            write!(&mut output, "<anonymous>").unwrap();
        }

        write!(&mut output, " @ {}", frame.module_url).unwrap();

        writeln!(&mut output, ":{:#06x}", frame.pc).unwrap();

        // Current instruction
        if let Some(inst) = &frame.instruction {
            writeln!(&mut output, "       {}", inst).unwrap();
        }

        writeln!(&mut output).unwrap();
    }

    output
}

/// Format trace buffer with loop detection analysis and compression
pub fn format_trace_buffer(buffer: &TraceRingBuffer) -> String {
    let mut output = String::new();
    let entries: Vec<&TraceEntry> = buffer.iter().collect();

    if entries.is_empty() {
        return output;
    }

    // Display recent instructions with compression for repeated patterns
    let mut i = 0;
    while i < entries.len() {
        let entry = entries[i];

        // Check if this instruction repeats
        let repeat_count = count_consecutive_repeats(&entries, i);

        if repeat_count > 3 {
            // Show first occurrence
            writeln!(
                &mut output,
                "  {:<8}: {} {}",
                entry.instruction_number,
                entry.opcode,
                shorten_operands(&entry.operands)
            ).unwrap();

            // Show repetition notice
            let last_repeat = i + repeat_count - 1;
            writeln!(
                &mut output,
                "  ... repeated {} more times (until #{})",
                repeat_count - 1,
                entries[last_repeat].instruction_number
            ).unwrap();

            i += repeat_count;
        } else {
            // Show normal entry
            writeln!(
                &mut output,
                "  {:<8}: {} {}",
                entry.instruction_number,
                entry.opcode,
                shorten_operands(&entry.operands)
            ).unwrap();

            // Show modified registers if any
            if !entry.modified_registers.is_empty() {
                for (reg, val) in &entry.modified_registers {
                    writeln!(&mut output, "             r{} := {}", reg, truncate_value(val, 40)).unwrap();
                }
            }
            i += 1;
        }
    }

    // Loop detection analysis
    if let Some(analysis) = detect_loops(&entries) {
        writeln!(&mut output).unwrap();
        writeln!(&mut output, "Analysis:").unwrap();
        write!(&mut output, "{}", analysis).unwrap();
    }

    output
}

/// Count consecutive repetitions of the same instruction pattern
fn count_consecutive_repeats(entries: &[&TraceEntry], start: usize) -> usize {
    if start >= entries.len() {
        return 0;
    }

    let first = entries[start];
    let mut count = 1;

    for i in (start + 1)..entries.len() {
        let entry = entries[i];
        // Check if opcode and operands match (ignore instruction_number and pc)
        if entry.opcode == first.opcode && entry.operands == first.operands {
            count += 1;
        } else {
            break;
        }
    }

    count
}

/// Shorten operands for more readable output
fn shorten_operands(operands: &str) -> String {
    // Remove "{ " and " }" wrapping
    let trimmed = operands.trim_start_matches(|c: char| c == '{' || c.is_whitespace())
                          .trim_end_matches(|c: char| c == '}' || c.is_whitespace());

    // Truncate if too long
    truncate_value(trimmed, 60).to_string()
}

/// Truncate a value string to max length
fn truncate_value(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len.saturating_sub(3)]
    }
}

/// Detect tight loops in instruction sequence
fn detect_loops(entries: &[&TraceEntry]) -> Option<String> {
    if entries.len() < 10 {
        return None;
    }

    // Look for repeated instruction patterns
    // Simple heuristic: if we see the same PC repeatedly in recent history
    let mut pc_counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    // Look at last 50 instructions
    let start = entries.len().saturating_sub(50);
    for entry in &entries[start..] {
        *pc_counts.entry(entry.pc).or_insert(0) += 1;
    }

    // Find most frequent PC
    let max_count = pc_counts.values().max().copied().unwrap_or(0);

    if max_count > 10 {
        let mut analysis = String::new();
        writeln!(&mut analysis, "  - Tight loop detected").unwrap();
        writeln!(&mut analysis, "  - Repeated instruction execution (max: {} times)", max_count).unwrap();
        writeln!(&mut analysis, "  - Possible infinite loop or missing termination condition").unwrap();
        writeln!(&mut analysis).unwrap();
        writeln!(&mut analysis, "Suggested Actions:").unwrap();
        writeln!(&mut analysis, "  1. Check loop termination condition").unwrap();
        writeln!(&mut analysis, "  2. Verify callback function logic").unwrap();
        writeln!(&mut analysis, "  3. Look for missing break/return statements").unwrap();

        return Some(analysis);
    }

    // Look for jump patterns (back-jumps in sequence)
    let back_jumps = entries.iter()
        .filter(|e| e.opcode.contains("Jump"))
        .count();

    if back_jumps > entries.len() / 3 {
        let mut analysis = String::new();
        writeln!(&mut analysis, "  - High frequency of jump instructions ({} jumps in {} instructions)",
                 back_jumps, entries.len()).unwrap();
        writeln!(&mut analysis, "  - May indicate loop-heavy code or complex control flow").unwrap();

        return Some(analysis);
    }

    None
}

/// Format register state (showing only active registers)
pub fn format_registers(regs: &[(u16, String)]) -> String {
    let mut output = String::new();

    for (reg, val) in regs {
        // Truncate long values
        let display_val = if val.len() > 80 {
            format!("{}...", &val[..77])
        } else {
            val.clone()
        };

        writeln!(&mut output, "  r{}: {}", reg, display_val).unwrap();
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{TraceEntry, TraceRingBuffer};

    #[test]
    fn test_format_trace_buffer() {
        let mut buffer = TraceRingBuffer::new(5);

        buffer.push(TraceEntry {
            instruction_number: 1,
            pc: 100,
            function_index: 0,
            function_name: Some("test".to_string()),
            module_url: "test.js".to_string(),
            opcode: "LoadConst".to_string(),
            operands: "r0, 42".to_string(),
            modified_registers: vec![(0, "42".to_string())],
            execution_time_ns: None,
        });

        buffer.push(TraceEntry {
            instruction_number: 2,
            pc: 104,
            function_index: 0,
            function_name: Some("test".to_string()),
            module_url: "test.js".to_string(),
            opcode: "Return".to_string(),
            operands: "r0".to_string(),
            modified_registers: vec![],
            execution_time_ns: None,
        });

        let formatted = format_trace_buffer(&buffer);
        assert!(formatted.contains("LoadConst"));
        assert!(formatted.contains("Return"));
        assert!(formatted.contains("r0 := 42"));
    }

    #[test]
    fn test_loop_detection() {
        let mut buffer = TraceRingBuffer::new(50);

        // Simulate tight loop - same PC repeated many times
        for i in 0..30 {
            buffer.push(TraceEntry {
                instruction_number: i,
                pc: 200, // Same PC
                function_index: 0,
                function_name: Some("loop".to_string()),
                module_url: "test.js".to_string(),
                opcode: "Jump".to_string(),
                operands: "offset: -10".to_string(),
                modified_registers: vec![],
                execution_time_ns: None,
            });
        }

        let formatted = format_trace_buffer(&buffer);
        assert!(formatted.contains("Tight loop detected"));
    }
}
