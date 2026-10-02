//! Abrupt completions as ordinary control flow, and the exception handler
//! table.
//!
//! `break`, `continue`, and `return` leave through every construct between
//! them and their target. A `finally` block is entered with a completion
//! token naming the command to resume after it, and an iterator is closed
//! on its own exit path outside the loop body, so no completion is ever
//! parked at run time. Throws reach handlers through the function's static
//! handler table.
//!
//! # Contents
//! - [`Command`] — one abrupt completion: `break`, `continue`, or `return`.
//! - [`ControlScope`] — one construct an abrupt completion must pass.
//! - [`FunctionContext::emit_abrupt`] — route a command from the current
//!   point to its target.
//! - Handler-table emission: [`FunctionContext::protect`] and
//!   [`FunctionContext::patch_handler_to_here`].
//! - Finally and iterator scopes: entry, the token dispatch after a finally
//!   block, and the close-then-continue exits after a loop.
//!
//! # Invariants
//! - A routed command executes nothing that can throw at its origin: the
//!   finally block, the iterator close, and the rest of the route run at the
//!   construct's own exits, outside every handler range the origin sits in
//!   but inside the ones enclosing the construct.
//! - Finally tokens: [`NORMAL_TOKEN`] for falling out of the protected
//!   range, [`THROW_TOKEN`] for a caught throw held in the scope's value
//!   register, and [`FIRST_COMMAND_TOKEN`]` + i` for the scope's `i`-th
//!   recorded command. A `return` carries its value in the value register.
//! - Handler entries are recorded innermost first: a range is closed before
//!   any range enclosing it.
//!
//! # See also
//! - `try_catch` and `for_loops` for the constructs that open scopes.
//! - `otter_bytecode::ExceptionHandler` for the table format.

use crate::*;
use otter_bytecode::ExceptionHandler;

/// Token of a finally block entered by falling out of its protected range.
pub(crate) const NORMAL_TOKEN: i32 = 0;
/// Token of a finally block entered by a throw.
pub(crate) const THROW_TOKEN: i32 = 1;
/// Token of the first command recorded by a finally scope.
pub(crate) const FIRST_COMMAND_TOKEN: i32 = 2;

/// One abrupt completion leaving the current point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    /// `break` to `loops[index]`.
    Break(usize),
    /// `continue` to `loops[index]`.
    Continue(usize),
    /// `return` from the function.
    Return,
}

/// A `finally` block the current point is protected by.
#[derive(Debug)]
pub(crate) struct FinallyScope {
    /// Register holding the completion token on entry to the block.
    pub(crate) token: u16,
    /// Register holding a thrown value or a returned value.
    pub(crate) value: u16,
    /// Jumps into the block from routed commands.
    pub(crate) entries: Vec<u32>,
    /// Commands routed through the block, in token order.
    pub(crate) commands: Vec<Command>,
}

/// An iterator closed when control leaves its construct abruptly.
#[derive(Debug)]
pub(crate) struct IteratorScope {
    /// Register holding the iterator.
    pub(crate) iterator: u16,
    /// `true` for an async iterator, whose close awaits.
    pub(crate) is_async: bool,
    /// Register holding a returned value on the way out.
    pub(crate) value: u16,
    /// Jumps to the close-then-continue exit of each command.
    pub(crate) exits: Vec<(Command, Vec<u32>)>,
}

/// One construct between the current point and the function boundary.
#[derive(Debug)]
pub(crate) enum ControlScope {
    /// A `try` block with a catch clause and no finally: commands pass it
    /// unchanged, but a call inside it is not in tail position.
    Catch,
    /// A `try` block (or catch clause) with a finally block.
    Finally(FinallyScope),
    /// A loop or destructuring pattern holding an open iterator.
    Iterator(IteratorScope),
}

impl FunctionContext {
    /// Whether no construct between here and the function boundary needs
    /// the frame after a `return`.
    pub(crate) fn return_leaves_directly(&self) -> bool {
        self.control.is_empty()
    }

    /// Whether a throw here can land in a handler of this function, where
    /// code after it still observes the frame's registers.
    pub(crate) fn in_protected_range(&self) -> bool {
        self.control
            .iter()
            .any(|scope| matches!(scope, ControlScope::Catch | ControlScope::Finally(_)))
    }

    /// Record a handler for throws raised by instructions in
    /// `start..next_pc`, landing in register `exception`. Returns its index
    /// for [`Self::patch_handler_to_here`], or `None` when the range is
    /// empty and nothing can throw into it.
    pub(crate) fn protect(&mut self, start: u32, exception: u16) -> Option<usize> {
        let end = self.next_pc();
        if start == end {
            return None;
        }
        self.handlers.push(ExceptionHandler {
            start,
            end,
            target: end,
            exception,
        });
        Some(self.handlers.len() - 1)
    }

    /// Land handler `index` at the next instruction.
    pub(crate) fn patch_handler_to_here(&mut self, index: usize) {
        let target = self.next_pc();
        self.handlers[index].target = target;
    }

    /// Route `command` from here to its target. `value` is the returned
    /// register of a `return`; `None` returns `undefined`.
    pub(crate) fn emit_abrupt(&mut self, command: Command, value: Option<u16>, span: (u32, u32)) {
        let depth = match command {
            Command::Break(target) => self.loops[target].break_depth,
            Command::Continue(target) => self.loops[target].continue_depth,
            Command::Return => 0,
        };
        let mut index = self.control.len();
        while index > depth {
            index -= 1;
            let carried = match &self.control[index] {
                ControlScope::Catch => continue,
                ControlScope::Finally(scope) => scope.value,
                ControlScope::Iterator(scope) => scope.value,
            };
            if command == Command::Return {
                self.emit_move(carried, value, span);
            }
            match &self.control[index] {
                ControlScope::Finally(scope) => {
                    let token = scope.token;
                    let slot = scope
                        .commands
                        .iter()
                        .position(|known| *known == command)
                        .unwrap_or(scope.commands.len());
                    self.emit(
                        Op::LoadInt32,
                        [
                            Operand::Register(token),
                            Operand::Imm32(FIRST_COMMAND_TOKEN + slot as i32),
                        ],
                        span,
                    );
                    let jump = self.emit_branch_placeholder(Op::Jump, None, span);
                    let ControlScope::Finally(scope) = &mut self.control[index] else {
                        unreachable!("scope kind checked above");
                    };
                    if slot == scope.commands.len() {
                        scope.commands.push(command);
                    }
                    scope.entries.push(jump);
                }
                ControlScope::Iterator(_) => {
                    let jump = self.emit_branch_placeholder(Op::Jump, None, span);
                    let ControlScope::Iterator(scope) = &mut self.control[index] else {
                        unreachable!("scope kind checked above");
                    };
                    match scope.exits.iter_mut().find(|(known, _)| *known == command) {
                        Some((_, jumps)) => jumps.push(jump),
                        None => scope.exits.push((command, vec![jump])),
                    }
                }
                ControlScope::Catch => unreachable!("skipped above"),
            }
            return;
        }
        match command {
            Command::Break(target) => {
                let jump = self.emit_branch_placeholder(Op::Jump, None, span);
                self.loops[target].break_patches.push(jump);
            }
            Command::Continue(target) => {
                let jump = self.emit_branch_placeholder(Op::Jump, None, span);
                self.loops[target].continue_patches.push(jump);
            }
            Command::Return => match value {
                Some(value) => self.emit(Op::ReturnValue, [Operand::Register(value)], span),
                None => self.emit(Op::ReturnUndefined, [], span),
            },
        }
    }

    /// `dst = src`, or `dst = undefined` without a source.
    fn emit_move(&mut self, dst: u16, src: Option<u16>, span: (u32, u32)) {
        match src {
            Some(src) if src == dst => {}
            Some(src) => self.emit(
                Op::LoadLocal,
                [Operand::Register(dst), Operand::Imm32(i32::from(src))],
                span,
            ),
            None => self.emit(Op::LoadUndefined, [Operand::Register(dst)], span),
        }
    }

    /// Open a finally scope over the code that follows. Returns the PC its
    /// protected range starts at.
    pub(crate) fn enter_finally(&mut self) -> u32 {
        let token = self.alloc_scratch();
        let value = self.alloc_scratch();
        self.control.push(ControlScope::Finally(FinallyScope {
            token,
            value,
            entries: Vec::new(),
            commands: Vec::new(),
        }));
        self.next_pc()
    }

    /// Close the finally scope opened at `start`: falling out of the range
    /// enters the block with [`NORMAL_TOKEN`], a throw with
    /// [`THROW_TOKEN`]. Returns the scope for [`Self::emit_finally_exits`];
    /// the block's code follows this call.
    pub(crate) fn leave_finally_range(&mut self, start: u32, span: (u32, u32)) -> FinallyScope {
        let Some(ControlScope::Finally(scope)) = self.control.pop() else {
            unreachable!("finally scope is innermost when its range ends");
        };
        let handler = self.protect(start, scope.value);
        self.emit(
            Op::LoadInt32,
            [Operand::Register(scope.token), Operand::Imm32(NORMAL_TOKEN)],
            span,
        );
        let mut entries = scope.entries;
        if let Some(handler) = handler {
            entries.push(self.emit_branch_placeholder(Op::Jump, None, span));
            self.patch_handler_to_here(handler);
            self.emit(
                Op::LoadInt32,
                [Operand::Register(scope.token), Operand::Imm32(THROW_TOKEN)],
                span,
            );
        }
        for jump in entries {
            self.patch_branch_to_here(jump);
        }
        FinallyScope {
            entries: Vec::new(),
            ..scope
        }
    }

    /// After a finally block: resume what entered it. A normal entry falls
    /// through; a throw is rethrown; each command continues from outside the
    /// scope.
    pub(crate) fn emit_finally_exits(&mut self, scope: FinallyScope, span: (u32, u32)) {
        let normal = self.emit_branch_placeholder(Op::JumpIfFalse, Some(scope.token), span);
        let test = self.alloc_scratch();
        for (slot, command) in scope.commands.iter().enumerate() {
            self.emit(
                Op::EqualImm,
                [
                    Operand::Register(test),
                    Operand::Register(scope.token),
                    Operand::Imm32(FIRST_COMMAND_TOKEN + slot as i32),
                ],
                span,
            );
            let other = self.emit_branch_placeholder(Op::JumpIfFalse, Some(test), span);
            let value = (*command == Command::Return).then_some(scope.value);
            self.emit_abrupt(*command, value, span);
            self.patch_branch_to_here(other);
        }
        // Every remaining token is a throw.
        self.emit(Op::Throw, [Operand::Register(scope.value)], span);
        self.patch_branch_to_here(normal);
    }

    /// Open an iterator scope for `iterator`; abrupt completions crossing it
    /// close the iterator first.
    pub(crate) fn enter_iterator_scope(&mut self, iterator: u16, is_async: bool) {
        let value = self.alloc_scratch();
        self.control.push(ControlScope::Iterator(IteratorScope {
            iterator,
            is_async,
            value,
            exits: Vec::new(),
        }));
    }

    /// Close the innermost iterator scope. Control must not fall into the
    /// code emitted here: each routed command gets an exit that closes the
    /// iterator and continues from outside the scope.
    pub(crate) fn leave_iterator_scope(&mut self, span: (u32, u32)) {
        let Some(ControlScope::Iterator(scope)) = self.control.pop() else {
            unreachable!("iterator scope is innermost when its construct ends");
        };
        for (command, jumps) in scope.exits {
            for jump in jumps {
                self.patch_branch_to_here(jump);
            }
            if scope.is_async {
                crate::for_loops::emit_async_iterator_close(self, scope.iterator, true, span);
            } else {
                self.emit(Op::IteratorClose, [Operand::Register(scope.iterator)], span);
            }
            let value = (command == Command::Return).then_some(scope.value);
            self.emit_abrupt(command, value, span);
        }
    }
}
