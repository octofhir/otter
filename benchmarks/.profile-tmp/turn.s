
target/release/otter:	file format mach-o arm64

Disassembly of section __TEXT,__text:

0000000100fb4cec <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn>:
100fb4cec:     	stp	x28, x27, [sp, #-0x60]!
100fb4cf0:     	stp	x26, x25, [sp, #0x10]
100fb4cf4:     	stp	x24, x23, [sp, #0x20]
100fb4cf8:     	stp	x22, x21, [sp, #0x30]
100fb4cfc:     	stp	x20, x19, [sp, #0x40]
100fb4d00:     	stp	x29, x30, [sp, #0x50]
100fb4d04:     	add	x29, sp, #0x50
100fb4d08:     	sub	sp, sp, #0x1c0
100fb4d0c:     	ldr	x8, [x0]
100fb4d10:     	cbz	x8, 0x100fb4de0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf4>
100fb4d14:     	ldr	x8, [x8, #0x8]
100fb4d18:     	cbz	x8, 0x100fb4de0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf4>
100fb4d1c:     	mov	x20, x0
100fb4d20:     	ldr	x9, [x0, #0x8]
100fb4d24:     	cbz	x9, 0x100fb4de0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf4>
100fb4d28:     	mov	x28, x1
100fb4d2c:     	ldp	x21, x22, [x8]
100fb4d30:     	ldr	x8, [x8, #0x10]
100fb4d34:     	ldr	w2, [x9]
100fb4d38:     	cbz	x8, 0x100fb4dc4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd8>
100fb4d3c:     	ldr	x10, [x8]
100fb4d40:     	ldr	w9, [x8, #0x10]
100fb4d44:     	subs	w9, w2, w9
100fb4d48:     	b.lo	0x100fb4d70 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x84>
100fb4d4c:     	ldr	x11, [x10, #0x20]
100fb4d50:     	ldr	x11, [x11, #0x50]
100fb4d54:     	cmp	x11, x9
100fb4d58:     	b.ls	0x100fb4d70 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x84>
100fb4d5c:     	ldr	x10, [x10, #0x28]
100fb4d60:     	ldr	x11, [x10, #0x60]
100fb4d64:     	cmp	x11, x9
100fb4d68:     	b.hi	0x100fb4d94 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa8>
100fb4d6c:     	b	0x100fb4da4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb8>
100fb4d70:     	ldr	x10, [x10, #0x28]
100fb4d74:     	ldp	w9, w11, [x10, #0x10]
100fb4d78:     	cmp	w9, #0x0
100fb4d7c:     	ccmp	w11, w2, #0x0, ne
100fb4d80:     	b.ne	0x100fb4dc4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd8>
100fb4d84:     	mov	x9, #0x0                ; =0
100fb4d88:     	ldr	x11, [x10, #0x60]
100fb4d8c:     	cmp	x11, x9
100fb4d90:     	b.ls	0x100fb4da4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb8>
100fb4d94:     	ldr	x10, [x10, #0x58]
100fb4d98:     	add	x9, x10, x9, lsl #4
100fb4d9c:     	ldapr	x9, [x9]
100fb4da0:     	cbz	x9, 0x100fb4dc4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd8>
100fb4da4:     	stp	xzr, x8, [sp, #0x38]
100fb4da8:     	cbz	x21, 0x100fb4ed4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1e8>
100fb4dac:     	add	x8, sp, #0x38
100fb4db0:     	add	x9, x8, #0x8
100fb4db4:     	str	x9, [sp, #0x18]
100fb4db8:     	add	x23, x8, #0x10
100fb4dbc:     	cbnz	x22, 0x100fb4e2c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x140>
100fb4dc0:     	b	0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb4dc4:     	cbz	x21, 0x100fb4de0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf4>
100fb4dc8:     	add	x0, sp, #0x50
100fb4dcc:     	mov	x1, x21
100fb4dd0:     	bl	0x100db4aec <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm9microtask9ownershipNtB6_11Interpreter20stable_foreign_owner>
100fb4dd4:     	ldrb	w8, [sp, #0x50]
100fb4dd8:     	cmp	w8, #0xff
100fb4ddc:     	b.eq	0x100fb4e10 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x124>
100fb4de0:     	mov	w27, #0xa               ; =10
100fb4de4:     	mov	w19, #0x6               ; =6
100fb4de8:     	mov	x0, x27
100fb4dec:     	mov	x1, x19
100fb4df0:     	add	sp, sp, #0x1c0
100fb4df4:     	ldp	x29, x30, [sp, #0x50]
100fb4df8:     	ldp	x20, x19, [sp, #0x40]
100fb4dfc:     	ldp	x22, x21, [sp, #0x30]
100fb4e00:     	ldp	x24, x23, [sp, #0x20]
100fb4e04:     	ldp	x26, x25, [sp, #0x10]
100fb4e08:     	ldp	x28, x27, [sp], #0x60
100fb4e0c:     	ret
100fb4e10:     	ldr	x8, [sp, #0x58]
100fb4e14:     	add	x9, sp, #0x38
100fb4e18:     	add	x10, x9, #0x8
100fb4e1c:     	str	x10, [sp, #0x18]
100fb4e20:     	stp	xzr, x8, [sp, #0x38]
100fb4e24:     	add	x23, x9, #0x10
100fb4e28:     	cbz	x22, 0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb4e2c:     	tbz	w28, #0x0, 0x100fb4e4c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x160>
100fb4e30:     	ldr	x8, [x22, #0x180]
100fb4e34:     	cbz	x8, 0x100fb4e4c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x160>
100fb4e38:     	ldr	x8, [x8, #0x8]
100fb4e3c:     	cbz	x8, 0x100fb4e4c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x160>
100fb4e40:     	ldrb	w9, [x8, #0xb]
100fb4e44:     	and	w9, w9, #0xfffffffd
100fb4e48:     	strb	w9, [x8, #0xb]
100fb4e4c:     	ldrb	w8, [x22, #0x112]
100fb4e50:     	cmp	w8, #0xff
100fb4e54:     	b.ne	0x100fb4e84 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x198>
100fb4e58:     	add	x8, x22, #0x1c8
100fb4e5c:     	ldr	x9, [x22, #0x1c8]
100fb4e60:     	add	x10, x22, #0x190
100fb4e64:     	mov	w11, #0x1c8             ; =456
100fb4e68:     	mov	w12, #0x190             ; =400
100fb4e6c:     	cmp	x9, #0x8
100fb4e70:     	csel	x9, x12, x11, hi
100fb4e74:     	csel	x8, x10, x8, hi
100fb4e78:     	ldr	x9, [x22, x9]
100fb4e7c:     	cbz	x9, 0x100fb4e84 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x198>
100fb4e80:     	str	xzr, [x8]
100fb4e84:     	cbz	w28, 0x100fb4e94 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1a8>
100fb4e88:     	ldr	x8, [x22, #0x10]
100fb4e8c:     	cmp	x8, #0x2
100fb4e90:     	b.ne	0x100fb4ee0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1f4>
100fb4e94:     	str	xzr, [sp, #0x20]
100fb4e98:     	ldr	x0, [x20, #0x8]
100fb4e9c:     	str	x0, [sp, #0x30]
100fb4ea0:     	cbz	x0, 0x100fb4efc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x210>
100fb4ea4:     	ldr	w26, [x0, #0x50]
100fb4ea8:     	cbnz	w26, 0x100fb4f00 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x214>
100fb4eac:     	ldr	x8, [x0, #0x48]
100fb4eb0:     	cbz	x8, 0x100fb4ebc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1d0>
100fb4eb4:     	ldr	w8, [x8, #0x50]
100fb4eb8:     	cbz	w8, 0x100fb5d1c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1030>
100fb4ebc:     	ldrb	w9, [x0, #0xa]
100fb4ec0:     	cmp	w9, #0x3
100fb4ec4:     	cinc	w26, w8, ne
100fb4ec8:     	cbz	w26, 0x100fb4f00 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x214>
100fb4ecc:     	str	w26, [x0, #0x50]
100fb4ed0:     	b	0x100fb4f00 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x214>
100fb4ed4:     	mov	w19, #0x6               ; =6
100fb4ed8:     	mov	w27, #0xa               ; =10
100fb4edc:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb4ee0:     	mov	x0, x22
100fb4ee4:     	bl	0x100df7e54 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack15consume_pending>
100fb4ee8:     	str	x0, [sp, #0x20]
100fb4eec:     	str	x1, [sp, #0x8]
100fb4ef0:     	ldr	x0, [x20, #0x8]
100fb4ef4:     	str	x0, [sp, #0x30]
100fb4ef8:     	cbnz	x0, 0x100fb4ea4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1b8>
100fb4efc:     	mov	w26, #0x0               ; =0
100fb4f00:     	ldp	x27, x19, [x20, #0xd0]
100fb4f04:     	ldp	w24, w25, [x20, #0xe0]
100fb4f08:     	adrp	x8, 0x102360000 <_anon.a9c7e006df0e5ecefd49f424e604641d.4+0x270>
100fb4f0c:     	ldr	q0, [x8, #0x220]
100fb4f10:     	str	q0, [x20, #0xd0]
100fb4f14:     	movi	d0, #0x000000ffffffff
100fb4f18:     	str	d0, [x20, #0xe0]
100fb4f1c:     	cbz	w28, 0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f20:     	cmn	w24, #0x2
100fb4f24:     	b.eq	0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f28:     	ldr	x8, [sp, #0x20]
100fb4f2c:     	cbnz	x8, 0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f30:     	ldr	x8, [sp, #0x30]
100fb4f34:     	ldr	w8, [x8, #0x4]
100fb4f38:     	cbnz	w8, 0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f3c:     	ldr	x8, [sp, #0x30]
100fb4f40:     	ldrb	w8, [x8, #0xa]
100fb4f44:     	cbnz	w8, 0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f48:     	ldp	x8, x9, [sp, #0x38]
100fb4f4c:     	add	x10, sp, #0x38
100fb4f50:     	cmp	x8, #0x0
100fb4f54:     	csel	x10, x9, x10, eq
100fb4f58:     	ldr	x8, [sp, #0x30]
100fb4f5c:     	ldr	w2, [x8]
100fb4f60:     	str	x10, [sp, #0x28]
100fb4f64:     	ldr	x0, [x10]
100fb4f68:     	add	x8, x9, #0x10
100fb4f6c:     	csel	x8, x8, x23, eq
100fb4f70:     	ldr	w1, [x8]
100fb4f74:     	bl	0x100dc140c <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm17execution_contextNtB5_16ExecutionContext12lazy_pending>
100fb4f78:     	cbz	w0, 0x100fb4f9c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2b0>
100fb4f7c:     	mov	x0, x20
100fb4f80:     	mov	x1, x21
100fb4f84:     	mov	x2, x22
100fb4f88:     	ldp	x3, x4, [sp, #0x28]
100fb4f8c:     	bl	0x100fb206c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch15enter_lazy_body>
100fb4f90:     	mov	x27, x0
100fb4f94:     	mov	x19, x1
100fb4f98:     	b	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb4f9c:     	ldr	x9, [sp, #0x30]
100fb4fa0:     	ldrb	w8, [x9, #0xa]
100fb4fa4:     	cbz	w8, 0x100fb4fe8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2fc>
100fb4fa8:     	str	xzr, [sp, #0x10]
100fb4fac:     	mov	w8, w26
100fb4fb0:     	sub	x8, x8, #0x1
100fb4fb4:     	str	x8, [sp, #0x28]
100fb4fb8:     	cmn	w24, #0x2
100fb4fbc:     	b.ne	0x100fb51a8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4bc>
100fb4fc0:     	cmp	x19, #0x6
100fb4fc4:     	b.hi	0x100fb5074 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x388>
100fb4fc8:     	cmp	x19, #0x2
100fb4fcc:     	b.le	0x100fb5094 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x3a8>
100fb4fd0:     	sub	x8, x19, #0x3
100fb4fd4:     	cmp	x8, #0x3
100fb4fd8:     	b.lo	0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb4fdc:     	cmp	x27, #0xa
100fb4fe0:     	b.eq	0x100fb50c4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x3d8>
100fb4fe4:     	b	0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb4fe8:     	ldr	w8, [x9]
100fb4fec:     	ldp	x9, x10, [sp, #0x38]
100fb4ff0:     	add	x11, sp, #0x38
100fb4ff4:     	cmp	x9, #0x0
100fb4ff8:     	csel	x9, x10, x11, eq
100fb4ffc:     	ldr	x9, [x9]
100fb5000:     	add	x10, x10, #0x10
100fb5004:     	csel	x10, x10, x23, eq
100fb5008:     	ldr	w10, [x10]
100fb500c:     	subs	w1, w8, w10
100fb5010:     	b.lo	0x100fb502c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x340>
100fb5014:     	ldr	x10, [x9, #0x20]
100fb5018:     	ldr	x10, [x10, #0x50]
100fb501c:     	cmp	x10, x1
100fb5020:     	b.ls	0x100fb502c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x340>
100fb5024:     	ldr	x9, [x9, #0x28]
100fb5028:     	b	0x100fb5044 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x358>
100fb502c:     	ldr	x9, [x9, #0x28]
100fb5030:     	ldp	w10, w11, [x9, #0x10]
100fb5034:     	cmp	w10, #0x0
100fb5038:     	ccmp	w11, w8, #0x0, ne
100fb503c:     	b.ne	0x100fb4fa8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2bc>
100fb5040:     	mov	w1, #0x0                ; =0
100fb5044:     	add	x0, x9, #0x10
100fb5048:     	bl	0x100e20180 <__RNvMs_NtCsgTc3P8JF8aG_8otter_vm10executableNtB4_16ExecutableModule13function_slot>
100fb504c:     	cbz	x0, 0x100fb4fa8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2bc>
100fb5050:     	ldr	x8, [x0]
100fb5054:     	add	x8, x8, #0x10
100fb5058:     	str	x8, [sp, #0x10]
100fb505c:     	mov	w8, w26
100fb5060:     	sub	x8, x8, #0x1
100fb5064:     	str	x8, [sp, #0x28]
100fb5068:     	cmn	w24, #0x2
100fb506c:     	b.ne	0x100fb51a8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4bc>
100fb5070:     	b	0x100fb4fc0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x2d4>
100fb5074:     	ldr	x8, [sp, #0x30]
100fb5078:     	ldrb	w8, [x8, #0xa]
100fb507c:     	cbz	w8, 0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5080:     	mov	x25, x22
100fb5084:     	ldr	x8, [x25, #0x10]!
100fb5088:     	cmp	x8, #0x2
100fb508c:     	b.ne	0x100fb538c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6a0>
100fb5090:     	b	0x100fb528c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x5a0>
100fb5094:     	cbz	x19, 0x100fb50c4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x3d8>
100fb5098:     	cmp	x19, #0x1
100fb509c:     	b.ne	0x100fb50c4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x3d8>
100fb50a0:     	lsr	x8, x27, #42
100fb50a4:     	cbnz	x8, 0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb50a8:     	ubfx	x8, x27, #32, #8
100fb50ac:     	sub	w8, w8, #0x10
100fb50b0:     	cmn	w8, #0xf
100fb50b4:     	mov	x8, #0x30000000000      ; =3298534883328
100fb50b8:     	bic	x8, x8, x27
100fb50bc:     	ccmp	x8, #0x0, #0x4, hs
100fb50c0:     	b.eq	0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb50c4:     	ldr	x10, [x21, #0x7c50]
100fb50c8:     	ldr	x8, [x10, #0x88]
100fb50cc:     	cbz	x8, 0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb50d0:     	mov	x8, #0x0                ; =0
100fb50d4:     	mov	x9, #0xa9c5             ; =43461
100fb50d8:     	movk	x9, #0x2e62, lsl #16
100fb50dc:     	movk	x9, #0x7aea, lsl #32
100fb50e0:     	movk	x9, #0xf135, lsl #48
100fb50e4:     	mul	x9, x25, x9
100fb50e8:     	ror	x12, x9, #0x26
100fb50ec:     	lsr	x11, x12, #57
100fb50f0:     	ldp	x10, x9, [x10, #0x70]
100fb50f4:     	dup.8b	v0, w11
100fb50f8:     	movi.2d	v1, #0xffffffffffffffff
100fb50fc:     	mov	w11, #0x70              ; =112
100fb5100:     	and	x12, x12, x9
100fb5104:     	ldr	d2, [x10, x12]
100fb5108:     	cmeq.8b	v3, v2, v0
100fb510c:     	fmov	x13, d3
100fb5110:     	ands	x13, x13, #0x8080808080808080
100fb5114:     	b.eq	0x100fb5148 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x45c>
100fb5118:     	rbit	x14, x13
100fb511c:     	clz	x14, x14
100fb5120:     	add	x14, x12, x14, lsr #3
100fb5124:     	and	x14, x14, x9
100fb5128:     	mneg	x14, x14, x11
100fb512c:     	add	x14, x10, x14
100fb5130:     	ldur	x15, [x14, #-0x70]
100fb5134:     	cmp	x15, x25
100fb5138:     	b.eq	0x100fb5178 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x48c>
100fb513c:     	sub	x14, x13, #0x2
100fb5140:     	ands	x13, x14, x13
100fb5144:     	b.ne	0x100fb5118 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x42c>
100fb5148:     	cmeq.8b	v2, v2, v1
100fb514c:     	fmov	x13, d2
100fb5150:     	cbnz	x13, 0x100fb519c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4b0>
100fb5154:     	add	x8, x8, #0x8
100fb5158:     	add	x12, x12, x8
100fb515c:     	and	x12, x12, x9
100fb5160:     	ldr	d2, [x10, x12]
100fb5164:     	cmeq.8b	v3, v2, v0
100fb5168:     	fmov	x13, d3
100fb516c:     	ands	x13, x13, #0x8080808080808080
100fb5170:     	b.ne	0x100fb5118 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x42c>
100fb5174:     	b	0x100fb5148 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x45c>
100fb5178:     	ldp	x8, x9, [x14, #-0x50]
100fb517c:     	ldr	x10, [x9, #0x10]
100fb5180:     	sub	x10, x10, #0x1
100fb5184:     	and	x10, x10, #0xfffffffffffffff0
100fb5188:     	add	x8, x8, x10
100fb518c:     	ldr	x9, [x9, #0xb0]
100fb5190:     	add	x0, x8, #0x10
100fb5194:     	mov	x1, x19
100fb5198:     	blr	x9
100fb519c:     	ldr	x8, [sp, #0x30]
100fb51a0:     	ldrb	w8, [x8, #0xa]
100fb51a4:     	cbz	w8, 0x100fb52d8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x5ec>
100fb51a8:     	cmp	x27, #0xa
100fb51ac:     	b.ne	0x100fb51cc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4e0>
100fb51b0:     	cmp	x19, #0x6
100fb51b4:     	b.ne	0x100fb51cc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x4e0>
100fb51b8:     	ldr	x8, [x20, #0x10]
100fb51bc:     	cbz	x8, 0x100fb523c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x550>
100fb51c0:     	ldrb	w8, [x8]
100fb51c4:     	cmp	w8, #0xf
100fb51c8:     	b.ne	0x100fb523c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x550>
100fb51cc:     	mov	x25, x22
100fb51d0:     	ldr	x8, [x25, #0x10]!
100fb51d4:     	cmp	x8, #0x2
100fb51d8:     	b.ne	0x100fb538c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6a0>
100fb51dc:     	cmn	w24, #0x2
100fb51e0:     	b.eq	0x100fb528c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x5a0>
100fb51e4:     	ldr	x9, [sp, #0x30]
100fb51e8:     	ldrb	w26, [x9, #0xb]
100fb51ec:     	and	w8, w26, #0xfffffff7
100fb51f0:     	strb	w8, [x9, #0xb]
100fb51f4:     	cmp	x19, #0x6
100fb51f8:     	b.hi	0x100fb5314 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x628>
100fb51fc:     	cbz	x19, 0x100fb5350 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x664>
100fb5200:     	cmp	x19, #0x2
100fb5204:     	b.eq	0x100fb5324 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x638>
100fb5208:     	cmp	x19, #0x6
100fb520c:     	b.ne	0x100fb5314 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x628>
100fb5210:     	cmp	x27, #0xa
100fb5214:     	b.ne	0x100fb5314 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x628>
100fb5218:     	ldr	x8, [x20, #0x10]
100fb521c:     	cbz	x8, 0x100fb5234 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x548>
100fb5220:     	ldrb	w26, [x8]
100fb5224:     	mov	w9, #0xff               ; =255
100fb5228:     	strb	w9, [x8]
100fb522c:     	cmp	w26, #0xff
100fb5230:     	b.ne	0x100fb5ce0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xff4>
100fb5234:     	mov	w26, #0x1               ; =1
100fb5238:     	b	0x100fb5cf0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1004>
100fb523c:     	str	xzr, [x21, #0x108]
100fb5240:     	ldr	x8, [sp, #0x30]
100fb5244:     	ldr	w1, [x8, #0x64]
100fb5248:     	str	wzr, [x8, #0x64]
100fb524c:     	cbz	w1, 0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5250:     	mov	w8, #0x7548             ; =30024
100fb5254:     	add	x0, x21, x8
100fb5258:     	bl	0x100db7c24 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10cold_frameNtB5_13ColdFramePool7release>
100fb525c:     	mov	w27, #0xa               ; =10
100fb5260:     	mov	w19, #0x6               ; =6
100fb5264:     	ldr	x8, [sp, #0x38]
100fb5268:     	cbz	x8, 0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb526c:     	mov	x20, #-0x1              ; =-1
100fb5270:     	ldaddl	x20, x8, [x8]
100fb5274:     	cmp	x8, #0x1
100fb5278:     	b.ne	0x100fb5bd8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xeec>
100fb527c:     	dmb	ishld
100fb5280:     	add	x0, sp, #0x38
100fb5284:     	bl	0x100e5eb48 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space12ChunkPayloadE9drop_slowBK_>
100fb5288:     	b	0x100fb5bd8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xeec>
100fb528c:     	ldp	x8, x9, [sp, #0x38]
100fb5290:     	add	x10, sp, #0x38
100fb5294:     	cmp	x8, #0x0
100fb5298:     	csel	x3, x9, x10, eq
100fb529c:     	add	x0, sp, #0x50
100fb52a0:     	mov	x1, x21
100fb52a4:     	mov	x2, x22
100fb52a8:     	ldr	x4, [sp, #0x28]
100fb52ac:     	mov	x5, x27
100fb52b0:     	mov	x6, x19
100fb52b4:     	bl	0x100da711c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8jit_callNtB6_11Interpreter23complete_compiled_entry>
100fb52b8:     	ldrb	w26, [sp, #0x50]
100fb52bc:     	cmp	w26, #0xff
100fb52c0:     	b.ne	0x100fb52f8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x60c>
100fb52c4:     	ldr	w8, [sp, #0x58]
100fb52c8:     	tbz	w8, #0x0, 0x100fb5390 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6a4>
100fb52cc:     	mov	x19, #0x0               ; =0
100fb52d0:     	ldr	x27, [sp, #0x60]
100fb52d4:     	b	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb52d8:     	cbz	x19, 0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb52dc:     	cmp	x19, #0x2
100fb52e0:     	b.eq	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb52e4:     	cmp	x19, #0x6
100fb52e8:     	b.ne	0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb52ec:     	cmp	x27, #0xa
100fb52f0:     	b.ne	0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb52f4:     	b	0x100fb5260 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x574>
100fb52f8:     	ldur	q0, [sp, #0x51]
100fb52fc:     	str	q0, [sp, #0xa0]
100fb5300:     	ldr	x8, [sp, #0x60]
100fb5304:     	stur	x8, [sp, #0xaf]
100fb5308:     	ldr	x8, [sp, #0x20]
100fb530c:     	tbz	w8, #0x0, 0x100fb53ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6c0>
100fb5310:     	b	0x100fb5398 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ac>
100fb5314:     	mov	w26, #0x1               ; =1
100fb5318:     	ldr	x8, [sp, #0x20]
100fb531c:     	tbz	w8, #0x0, 0x100fb53ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6c0>
100fb5320:     	b	0x100fb5398 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ac>
100fb5324:     	mov	w8, #0x1                ; =1
100fb5328:     	stp	x8, x27, [x21, #0x108]
100fb532c:     	ldr	x0, [x21, #0x7550]
100fb5330:     	ldr	x1, [x21, #0x7558]
100fb5334:     	ldr	x8, [sp, #0x30]
100fb5338:     	ldr	w2, [x8, #0x64]
100fb533c:     	bl	0x100d2f700 <__RNvMNtCsgTc3P8JF8aG_8otter_vm9frame_opsNtB4_11Interpreter23abandon_pending_ladders>
100fb5340:     	mov	w26, #0x12              ; =18
100fb5344:     	ldr	x8, [sp, #0x20]
100fb5348:     	tbz	w8, #0x0, 0x100fb53ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6c0>
100fb534c:     	b	0x100fb5398 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ac>
100fb5350:     	cmn	w24, #0x1
100fb5354:     	b.eq	0x100fb5388 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x69c>
100fb5358:     	lsr	w8, w24, #16
100fb535c:     	cbnz	w8, 0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5360:     	add	x8, sp, #0x50
100fb5364:     	ldr	x0, [sp, #0x30]
100fb5368:     	bl	0x100e02248 <__RNvMs3_NtCsgTc3P8JF8aG_8otter_vm12active_frameNtB5_14ActiveFrameMut8from_ptr>
100fb536c:     	ldr	x8, [sp, #0x50]
100fb5370:     	cbz	x8, 0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5374:     	ldr	x8, [sp, #0x60]
100fb5378:     	cmp	x8, x24
100fb537c:     	b.ls	0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5380:     	ldr	x8, [sp, #0x58]
100fb5384:     	str	x27, [x8, x24, lsl #3]
100fb5388:     	tbnz	w26, #0x3, 0x100fb5cb8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfcc>
100fb538c:     	mov	w26, #0xff              ; =255
100fb5390:     	ldr	x8, [sp, #0x20]
100fb5394:     	tbz	w8, #0x0, 0x100fb53ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6c0>
100fb5398:     	mov	w8, #0x1                ; =1
100fb539c:     	str	x8, [x21, #0x108]
100fb53a0:     	ldr	x8, [sp, #0x8]
100fb53a4:     	str	x8, [x21, #0x110]
100fb53a8:     	mov	w26, #0x12              ; =18
100fb53ac:     	tbz	w28, #0x0, 0x100fb5578 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x88c>
100fb53b0:     	cmn	w24, #0x2
100fb53b4:     	b.eq	0x100fb53d8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ec>
100fb53b8:     	ldr	x8, [sp, #0x30]
100fb53bc:     	ldr	w8, [x8, #0x4]
100fb53c0:     	cbnz	w8, 0x100fb53d8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ec>
100fb53c4:     	add	x9, x21, #0x8, lsl #12  ; =0x8000
100fb53c8:     	ldr	x8, [x9, #0x158]
100fb53cc:     	adds	x8, x8, #0x1
100fb53d0:     	csinv	x8, x8, xzr, lo
100fb53d4:     	str	x8, [x9, #0x158]
100fb53d8:     	ldr	x8, [sp, #0x10]
100fb53dc:     	cbz	x8, 0x100fb542c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x740>
100fb53e0:     	cmp	w26, #0xff
100fb53e4:     	b.ne	0x100fb542c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x740>
100fb53e8:     	ldp	x8, x9, [sp, #0x38]
100fb53ec:     	add	x10, sp, #0x38
100fb53f0:     	cmp	x8, #0x0
100fb53f4:     	csel	x4, x9, x10, eq
100fb53f8:     	ldr	x2, [x22, #0x180]
100fb53fc:     	add	x0, sp, #0x50
100fb5400:     	mov	x1, x21
100fb5404:     	ldr	x3, [sp, #0x30]
100fb5408:     	ldr	x5, [sp, #0x10]
100fb540c:     	bl	0x100d9c7bc <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp6framesNtB6_11Interpreter28begin_suspendable_activation>
100fb5410:     	ldrb	w26, [sp, #0x50]
100fb5414:     	cmp	w26, #0xff
100fb5418:     	b.eq	0x100fb542c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x740>
100fb541c:     	ldur	q0, [sp, #0x51]
100fb5420:     	str	q0, [sp, #0xa0]
100fb5424:     	ldr	x8, [sp, #0x60]
100fb5428:     	stur	x8, [sp, #0xaf]
100fb542c:     	cmn	w24, #0x2
100fb5430:     	b.eq	0x100fb5578 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x88c>
100fb5434:     	ldr	x8, [sp, #0x30]
100fb5438:     	ldr	w8, [x8, #0x4]
100fb543c:     	cbnz	w8, 0x100fb544c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x760>
100fb5440:     	mov	x0, x21
100fb5444:     	ldr	x1, [sp, #0x30]
100fb5448:     	bl	0x100d82a20 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp14jit_retrainingNtB6_11Interpreter39begin_interpreted_retraining_activation>
100fb544c:     	cmp	w26, #0xff
100fb5450:     	b.ne	0x100fb5578 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x88c>
100fb5454:     	ldr	x8, [sp, #0x10]
100fb5458:     	cbz	x8, 0x100fb54d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x7e8>
100fb545c:     	ldr	x8, [sp, #0x30]
100fb5460:     	ldr	w8, [x8, #0x4]
100fb5464:     	cbnz	w8, 0x100fb54d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x7e8>
100fb5468:     	ldr	x8, [sp, #0x10]
100fb546c:     	ldrb	w8, [x8, #0x126]
100fb5470:     	cbz	w8, 0x100fb54d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x7e8>
100fb5474:     	ldp	x8, x9, [sp, #0x38]
100fb5478:     	add	x10, sp, #0x38
100fb547c:     	cmp	x8, #0x0
100fb5480:     	csel	x3, x9, x10, eq
100fb5484:     	add	x0, sp, #0x50
100fb5488:     	mov	x1, x21
100fb548c:     	mov	x2, x22
100fb5490:     	ldr	x4, [sp, #0x30]
100fb5494:     	bl	0x100fb4a90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch15link_asm_module>
100fb5498:     	ldrb	w26, [sp, #0x50]
100fb549c:     	cmp	w26, #0xff
100fb54a0:     	b.ne	0x100fb5568 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x87c>
100fb54a4:     	ldr	x8, [sp, #0x58]
100fb54a8:     	cbz	x8, 0x100fb54d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x7e8>
100fb54ac:     	ldr	x27, [sp, #0x60]
100fb54b0:     	ldr	x8, [sp, #0x30]
100fb54b4:     	ldr	w1, [x8, #0x64]
100fb54b8:     	str	wzr, [x8, #0x64]
100fb54bc:     	cbz	w1, 0x100fb54cc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x7e0>
100fb54c0:     	mov	w8, #0x7548             ; =30024
100fb54c4:     	add	x0, x21, x8
100fb54c8:     	bl	0x100db7c24 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10cold_frameNtB5_13ColdFramePool7release>
100fb54cc:     	mov	x19, #0x0               ; =0
100fb54d0:     	b	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb54d4:     	ldr	x8, [x25]
100fb54d8:     	cmp	x8, #0x2
100fb54dc:     	b.ne	0x100fb5560 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x874>
100fb54e0:     	ldr	x8, [x21, #0x7f30]
100fb54e4:     	cbz	x8, 0x100fb5560 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x874>
100fb54e8:     	ldp	x8, x9, [sp, #0x38]
100fb54ec:     	add	x10, sp, #0x38
100fb54f0:     	cmp	x8, #0x0
100fb54f4:     	csel	x3, x9, x10, eq
100fb54f8:     	add	x0, sp, #0x50
100fb54fc:     	mov	x1, x21
100fb5500:     	mov	x2, x22
100fb5504:     	bl	0x100da648c <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8jit_callNtB6_11Interpreter22prepare_compiled_entry>
100fb5508:     	ldrb	w26, [sp, #0x50]
100fb550c:     	cmp	w26, #0xff
100fb5510:     	b.ne	0x100fb5568 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x87c>
100fb5514:     	ldr	w8, [sp, #0x58]
100fb5518:     	tbz	w8, #0x0, 0x100fb5578 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x88c>
100fb551c:     	mov	x27, #0x0               ; =0
100fb5520:     	adrp	x8, 0x102d13000 <_anon.49087316163ed4345739c0711faf6653.2247+0x2889>
100fb5524:     	add	x8, x8, #0xfc0
100fb5528:     	ldp	q0, q1, [x8]
100fb552c:     	stp	q0, q1, [x20, #0x50]
100fb5530:     	ldp	q0, q1, [x8, #0x40]
100fb5534:     	stp	q0, q1, [x20, #0x90]
100fb5538:     	ldp	q0, q1, [x8, #0x60]
100fb553c:     	stp	q0, q1, [x20, #0xb0]
100fb5540:     	ldp	q0, q1, [x8, #0x20]
100fb5544:     	ldr	x8, [sp, #0x60]
100fb5548:     	str	x8, [x20, #0x50]
100fb554c:     	mov	w8, #0x80               ; =128
100fb5550:     	strb	w8, [x20, #0x63]
100fb5554:     	mov	w19, #0x3               ; =3
100fb5558:     	stp	q0, q1, [x20, #0x70]
100fb555c:     	b	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb5560:     	mov	w26, #0xff              ; =255
100fb5564:     	b	0x100fb5578 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x88c>
100fb5568:     	ldur	q0, [sp, #0x51]
100fb556c:     	str	q0, [sp, #0xa0]
100fb5570:     	ldr	x8, [sp, #0x60]
100fb5574:     	stur	x8, [sp, #0xaf]
100fb5578:     	ldp	x8, x9, [sp, #0x38]
100fb557c:     	add	x10, sp, #0x38
100fb5580:     	cmp	x8, #0x0
100fb5584:     	csel	x27, x9, x10, eq
100fb5588:     	ldr	x1, [x27]
100fb558c:     	ldr	x8, [x1, #0x28]
100fb5590:     	add	x19, x8, #0x10
100fb5594:     	ldr	x8, [x21, #0x7478]
100fb5598:     	cmp	x8, x19
100fb559c:     	b.eq	0x100fb55c8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x8dc>
100fb55a0:     	ldr	x9, [x21, #0x7480]
100fb55a4:     	cmp	x9, x19
100fb55a8:     	b.eq	0x100fb55c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x8d4>
100fb55ac:     	mov	w8, #0x7410             ; =29712
100fb55b0:     	add	x0, x21, x8
100fb55b4:     	mov	x2, x19
100fb55b8:     	bl	0x100da4d40 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm6interp8feedbackNtB2_23MethodFeedbackDirectory19install_new_context>
100fb55bc:     	ldr	x8, [x21, #0x7478]
100fb55c0:     	str	x19, [x21, #0x7478]
100fb55c4:     	str	x8, [x21, #0x7480]
100fb55c8:     	add	x28, sp, #0xc0
100fb55cc:     	sub	x19, x29, #0xd0
100fb55d0:     	sub	x8, x29, #0x80
100fb55d4:     	orr	x25, x8, #0x1
100fb55d8:     	mov	w8, #0xff               ; =255
100fb55dc:     	sturb	w8, [x29, #-0x80]
100fb55e0:     	ldr	q0, [sp, #0xa0]
100fb55e4:     	str	q0, [sp, #0x50]
100fb55e8:     	ldur	x8, [sp, #0xaf]
100fb55ec:     	stur	x8, [sp, #0x5f]
100fb55f0:     	ldr	q0, [x25]
100fb55f4:     	str	q0, [sp, #0xa0]
100fb55f8:     	ldur	x8, [x25, #0xf]
100fb55fc:     	stur	x8, [sp, #0xaf]
100fb5600:     	cmp	w26, #0xff
100fb5604:     	b.ne	0x100fb5684 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x998>
100fb5608:     	mov	w8, #-0x1               ; =-1
100fb560c:     	stur	w8, [x29, #-0xb0]
100fb5610:     	stur	xzr, [x29, #-0x98]
100fb5614:     	sub	x0, x29, #0xd0
100fb5618:     	sub	x6, x29, #0xb0
100fb561c:     	sub	x7, x29, #0x98
100fb5620:     	mov	x1, x21
100fb5624:     	mov	x2, x27
100fb5628:     	ldr	x3, [sp, #0x10]
100fb562c:     	mov	x4, x22
100fb5630:     	ldr	x5, [sp, #0x28]
100fb5634:     	bl	0x100dcce00 <__RNvMs0_NtNtCsgTc3P8JF8aG_8otter_vm6interp8dispatchNtB9_11Interpreter14dispatch_ticks>
100fb5638:     	ldur	x8, [x29, #-0x98]
100fb563c:     	add	x10, x21, #0x8, lsl #12 ; =0x8000
100fb5640:     	ldr	x9, [x10, #0x108]
100fb5644:     	add	x8, x9, x8
100fb5648:     	str	x8, [x10, #0x108]
100fb564c:     	ldur	w24, [x29, #-0xd0]
100fb5650:     	tbnz	w24, #0x0, 0x100fb5670 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x984>
100fb5654:     	ldurb	w8, [x29, #-0xc8]
100fb5658:     	sub	w9, w8, #0x17
100fb565c:     	cmp	w8, #0x16
100fb5660:     	mov	w10, #0x3               ; =3
100fb5664:     	csel	w9, w9, w10, hi
100fb5668:     	cmp	w9, #0x1
100fb566c:     	b.ne	0x100fb56ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x9c0>
100fb5670:     	ldur	w26, [x29, #-0xb0]
100fb5674:     	cmn	w26, #0x1
100fb5678:     	b.eq	0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb567c:     	eor	w19, w24, #0x1
100fb5680:     	b	0x100fb56c8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x9dc>
100fb5684:     	ldr	q0, [sp, #0x50]
100fb5688:     	stur	q0, [x19, #0x9]
100fb568c:     	ldur	x8, [sp, #0x5f]
100fb5690:     	stur	x8, [x19, #0x18]
100fb5694:     	sturb	w26, [x29, #-0xc8]
100fb5698:     	mov	w8, #0x1                ; =1
100fb569c:     	stur	x8, [x29, #-0xd0]
100fb56a0:     	ldr	x8, [sp, #0x20]
100fb56a4:     	mov	x24, x8
100fb56a8:     	b	0x100fb57b0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xac4>
100fb56ac:     	and	w9, w9, #0xff
100fb56b0:     	cmp	w9, #0x3
100fb56b4:     	b.ne	0x100fb5ad8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xdec>
100fb56b8:     	ldur	w26, [x29, #-0xb0]
100fb56bc:     	cmn	w26, #0x1
100fb56c0:     	b.eq	0x100fb5ad8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xdec>
100fb56c4:     	mov	w19, #0x0               ; =0
100fb56c8:     	ldr	x8, [x22, #0x180]
100fb56cc:     	cbz	x8, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb56d0:     	ldr	x0, [x8, #0x8]
100fb56d4:     	cbz	x0, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb56d8:     	ldrb	w8, [x0, #0xb]
100fb56dc:     	tbz	w8, #0x1, 0x100fb56e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x9fc>
100fb56e0:     	ldr	x0, [x0, #0x48]
100fb56e4:     	cbz	x0, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb56e8:     	ldrb	w8, [x0, #0xa]
100fb56ec:     	cmp	w8, #0x3
100fb56f0:     	b.eq	0x100fb56e0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x9f4>
100fb56f4:     	ldr	w8, [x0, #0x50]
100fb56f8:     	cbnz	w8, 0x100fb5730 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa44>
100fb56fc:     	ldr	x8, [x0, #0x48]
100fb5700:     	cbz	x8, 0x100fb5728 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa3c>
100fb5704:     	ldr	w8, [x8, #0x50]
100fb5708:     	cbz	w8, 0x100fb5758 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa6c>
100fb570c:     	adds	w8, w8, #0x1
100fb5710:     	b.lo	0x100fb572c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa40>
100fb5714:     	mov	w8, #0x0                ; =0
100fb5718:     	ldr	x9, [sp, #0x28]
100fb571c:     	cmp	x9, w8, uxtw
100fb5720:     	b.lo	0x100fb573c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa50>
100fb5724:     	b	0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb5728:     	mov	w8, #0x1                ; =1
100fb572c:     	str	w8, [x0, #0x50]
100fb5730:     	ldr	x9, [sp, #0x28]
100fb5734:     	cmp	x9, w8, uxtw
100fb5738:     	b.hs	0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb573c:     	ldr	x8, [x22, #0x180]
100fb5740:     	cbz	x8, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb5744:     	ldr	x8, [x8, #0x8]
100fb5748:     	cbz	x8, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb574c:     	ldrb	w9, [x8, #0xb]
100fb5750:     	tbz	w9, #0x1, 0x100fb5778 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa8c>
100fb5754:     	b	0x100fb5770 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa84>
100fb5758:     	bl	0x101b696c8 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100fb575c:     	mov	x8, x0
100fb5760:     	ldr	x9, [sp, #0x28]
100fb5764:     	cmp	x9, w8, uxtw
100fb5768:     	b.lo	0x100fb573c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa50>
100fb576c:     	b	0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb5770:     	ldr	x8, [x8, #0x48]
100fb5774:     	cbz	x8, 0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb5778:     	ldrb	w9, [x8, #0xa]
100fb577c:     	cmp	w9, #0x3
100fb5780:     	b.eq	0x100fb5770 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xa84>
100fb5784:     	ldr	w9, [x8, #0x4]
100fb5788:     	cmp	w9, w26
100fb578c:     	b.eq	0x100fb57a4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab8>
100fb5790:     	tbz	w19, #0x0, 0x100fb57a0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xab4>
100fb5794:     	ldrb	w9, [x8, #0xb]
100fb5798:     	orr	w9, w9, #0x8
100fb579c:     	strb	w9, [x8, #0xb]
100fb57a0:     	str	w26, [x8, #0x4]
100fb57a4:     	cbz	w24, 0x100fb5ad4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xde8>
100fb57a8:     	mov	w24, #0x0               ; =0
100fb57ac:     	sub	x19, x29, #0xd0
100fb57b0:     	ldur	q0, [x19, #0x8]
100fb57b4:     	str	q0, [x28, #0xa0]
100fb57b8:     	ldur	x8, [x19, #0x18]
100fb57bc:     	stur	x8, [x29, #-0xa0]
100fb57c0:     	ldurb	w19, [x29, #-0xb0]
100fb57c4:     	cmp	w19, #0x12
100fb57c8:     	b.ne	0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb57cc:     	ldr	x8, [x22, #0x180]
100fb57d0:     	cbz	x8, 0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb57d4:     	ldr	x0, [x8, #0x8]
100fb57d8:     	cbz	x0, 0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb57dc:     	ldrb	w8, [x0, #0xb]
100fb57e0:     	tbz	w8, #0x1, 0x100fb588c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xba0>
100fb57e4:     	b	0x100fb5884 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb98>
100fb57e8:     	sub	x0, x29, #0x98
100fb57ec:     	sub	x4, x29, #0xb0
100fb57f0:     	mov	x1, x21
100fb57f4:     	mov	x2, x27
100fb57f8:     	mov	x3, x22
100fb57fc:     	bl	0x100db27a8 <__RNvMNtNtCsgTc3P8JF8aG_8otter_vm9error_ops9throwableNtB6_11Interpreter38vm_error_to_throwable_with_stack_roots>
100fb5800:     	ldurb	w8, [x29, #-0x98]
100fb5804:     	cmp	w8, #0xff
100fb5808:     	b.ne	0x100fb5a8c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xda0>
100fb580c:     	cmp	w19, #0x14
100fb5810:     	b.eq	0x100fb581c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb30>
100fb5814:     	cmp	w19, #0x8
100fb5818:     	b.ne	0x100fb5834 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb48>
100fb581c:     	add	x8, sp, #0xc0
100fb5820:     	ldr	q0, [x8, #0xa0]
100fb5824:     	add	x28, sp, #0xc0
100fb5828:     	str	q0, [x8, #0xd0]
100fb582c:     	ldur	x8, [x29, #-0xa0]
100fb5830:     	stur	x8, [x29, #-0x70]
100fb5834:     	ldur	x5, [x29, #-0x90]
100fb5838:     	ldr	x3, [x22, #0x180]
100fb583c:     	add	x0, sp, #0x50
100fb5840:     	sub	x6, x29, #0x80
100fb5844:     	and	w7, w24, #0x1
100fb5848:     	mov	x1, x21
100fb584c:     	mov	x2, x27
100fb5850:     	ldr	x4, [sp, #0x28]
100fb5854:     	bl	0x100d2aaac <__RNvMNtCsgTc3P8JF8aG_8otter_vm9async_opsNtB4_11Interpreter32unwind_throw_with_uncaught_above>
100fb5858:     	ldrb	w8, [sp, #0x50]
100fb585c:     	cmp	w8, #0xff
100fb5860:     	b.ne	0x100fb5ab4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xdc8>
100fb5864:     	ldr	x8, [x22, #0x180]
100fb5868:     	sub	x19, x29, #0xd0
100fb586c:     	cbz	x8, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb5870:     	ldr	x0, [x8, #0x8]
100fb5874:     	cbz	x0, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb5878:     	ldrb	w8, [x0, #0xb]
100fb587c:     	tbz	w8, #0x1, 0x100fb59dc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcf0>
100fb5880:     	b	0x100fb59d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xce8>
100fb5884:     	ldr	x0, [x0, #0x48]
100fb5888:     	cbz	x0, 0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb588c:     	ldrb	w8, [x0, #0xa]
100fb5890:     	cmp	w8, #0x3
100fb5894:     	b.eq	0x100fb5884 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xb98>
100fb5898:     	ldr	w8, [x0, #0x50]
100fb589c:     	cbnz	w8, 0x100fb58d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xbe8>
100fb58a0:     	ldr	x8, [x0, #0x48]
100fb58a4:     	cbz	x8, 0x100fb58cc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xbe0>
100fb58a8:     	ldr	w8, [x8, #0x50]
100fb58ac:     	cbz	w8, 0x100fb5944 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xc58>
100fb58b0:     	adds	w8, w8, #0x1
100fb58b4:     	b.lo	0x100fb58d0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xbe4>
100fb58b8:     	mov	w8, #0x0                ; =0
100fb58bc:     	ldr	x9, [sp, #0x28]
100fb58c0:     	cmp	x9, w8, uxtw
100fb58c4:     	b.hs	0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb58c8:     	b	0x100fb58e0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xbf4>
100fb58cc:     	mov	w8, #0x1                ; =1
100fb58d0:     	str	w8, [x0, #0x50]
100fb58d4:     	ldr	x9, [sp, #0x28]
100fb58d8:     	cmp	x9, w8, uxtw
100fb58dc:     	b.hs	0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb58e0:     	ldp	x8, x23, [x21, #0x108]
100fb58e4:     	str	xzr, [x21, #0x108]
100fb58e8:     	cbz	x8, 0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb58ec:     	ldr	x3, [x22, #0x180]
100fb58f0:     	mov	w8, #0xff               ; =255
100fb58f4:     	strb	w8, [sp, #0x50]
100fb58f8:     	sub	x0, x29, #0x98
100fb58fc:     	add	x6, sp, #0x50
100fb5900:     	and	w7, w24, #0x1
100fb5904:     	mov	x1, x21
100fb5908:     	mov	x2, x27
100fb590c:     	ldr	x4, [sp, #0x28]
100fb5910:     	mov	x5, x23
100fb5914:     	bl	0x100d2aaac <__RNvMNtCsgTc3P8JF8aG_8otter_vm9async_opsNtB4_11Interpreter32unwind_throw_with_uncaught_above>
100fb5918:     	ldurb	w8, [x29, #-0x98]
100fb591c:     	cmp	w8, #0xff
100fb5920:     	b.ne	0x100fb5d00 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1014>
100fb5924:     	ldr	x8, [x22, #0x180]
100fb5928:     	sub	x19, x29, #0xd0
100fb592c:     	cbz	x8, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb5930:     	ldr	x0, [x8, #0x8]
100fb5934:     	cbz	x0, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb5938:     	ldrb	w8, [x0, #0xb]
100fb593c:     	tbz	w8, #0x1, 0x100fb5964 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xc78>
100fb5940:     	b	0x100fb595c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xc70>
100fb5944:     	bl	0x101b696c8 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100fb5948:     	mov	x8, x0
100fb594c:     	ldr	x9, [sp, #0x28]
100fb5950:     	cmp	x9, w8, uxtw
100fb5954:     	b.hs	0x100fb57e8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xafc>
100fb5958:     	b	0x100fb58e0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xbf4>
100fb595c:     	ldr	x0, [x0, #0x48]
100fb5960:     	cbz	x0, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb5964:     	ldrb	w8, [x0, #0xa]
100fb5968:     	cmp	w8, #0x3
100fb596c:     	b.eq	0x100fb595c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xc70>
100fb5970:     	ldr	w8, [x0, #0x50]
100fb5974:     	cbnz	w8, 0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb5978:     	ldr	x8, [x0, #0x48]
100fb597c:     	cbz	x8, 0x100fb599c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcb0>
100fb5980:     	ldr	w8, [x8, #0x50]
100fb5984:     	cbz	w8, 0x100fb59b4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcc8>
100fb5988:     	adds	w8, w8, #0x1
100fb598c:     	b.lo	0x100fb59a8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcbc>
100fb5990:     	mov	w8, #0x0                ; =0
100fb5994:     	sub	x19, x29, #0xd0
100fb5998:     	b	0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb599c:     	mov	w8, #0x1                ; =1
100fb59a0:     	str	w8, [x0, #0x50]
100fb59a4:     	b	0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb59a8:     	sub	x19, x29, #0xd0
100fb59ac:     	str	w8, [x0, #0x50]
100fb59b0:     	b	0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb59b4:     	bl	0x101b696c8 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100fb59b8:     	mov	x8, x0
100fb59bc:     	b	0x100fb5994 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xca8>
100fb59c0:     	ldr	x9, [sp, #0x28]
100fb59c4:     	cmp	x9, w8, uxtw
100fb59c8:     	b.hs	0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb59cc:     	mov	w26, #0xff              ; =255
100fb59d0:     	b	0x100fb55d8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x8ec>
100fb59d4:     	ldr	x0, [x0, #0x48]
100fb59d8:     	cbz	x0, 0x100fb5a24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd38>
100fb59dc:     	ldrb	w8, [x0, #0xa]
100fb59e0:     	cmp	w8, #0x3
100fb59e4:     	b.eq	0x100fb59d4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xce8>
100fb59e8:     	ldr	w8, [x0, #0x50]
100fb59ec:     	cbnz	w8, 0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb59f0:     	ldr	x8, [x0, #0x48]
100fb59f4:     	cbz	x8, 0x100fb5a10 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd24>
100fb59f8:     	ldr	w8, [x8, #0x50]
100fb59fc:     	cbz	w8, 0x100fb5a1c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd30>
100fb5a00:     	adds	w8, w8, #0x1
100fb5a04:     	b.lo	0x100fb5a14 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd28>
100fb5a08:     	mov	w8, #0x0                ; =0
100fb5a0c:     	b	0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb5a10:     	mov	w8, #0x1                ; =1
100fb5a14:     	str	w8, [x0, #0x50]
100fb5a18:     	b	0x100fb59c0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xcd4>
100fb5a1c:     	bl	0x101b696c8 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100fb5a20:     	b	0x100fb59b8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xccc>
100fb5a24:     	mov	x9, #0x0                ; =0
100fb5a28:     	ldr	w8, [x21, #0x118]
100fb5a2c:     	ldr	x10, [x21, #0x120]
100fb5a30:     	str	xzr, [x21, #0x118]
100fb5a34:     	cmp	w8, #0x0
100fb5a38:     	mov	w8, #0xa                ; =10
100fb5a3c:     	csel	x10, x10, x8, ne
100fb5a40:     	mov	w8, #0x17               ; =23
100fb5a44:     	sturb	w8, [x29, #-0xc8]
100fb5a48:     	stur	x10, [x29, #-0xc0]
100fb5a4c:     	ldr	x11, [sp, #0x30]
100fb5a50:     	cbz	x9, 0x100fb5af0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xe04>
100fb5a54:     	cmp	w8, #0x12
100fb5a58:     	b.ne	0x100fb5b78 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xe8c>
100fb5a5c:     	ldr	w8, [x21, #0x108]
100fb5a60:     	ldr	x9, [x21, #0x110]
100fb5a64:     	str	xzr, [x21, #0x108]
100fb5a68:     	cmp	w8, #0x0
100fb5a6c:     	mov	w8, #0x6                ; =6
100fb5a70:     	mov	w10, #0x2               ; =2
100fb5a74:     	csel	x19, x10, x8, ne
100fb5a78:     	mov	w8, #0xa                ; =10
100fb5a7c:     	csel	x27, x9, x8, ne
100fb5a80:     	ldr	x8, [sp, #0x38]
100fb5a84:     	cbnz	x8, 0x100fb5bbc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xed0>
100fb5a88:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5a8c:     	ldur	q0, [x29, #-0x98]
100fb5a90:     	sub	x19, x29, #0xd0
100fb5a94:     	stur	q0, [x19, #0x8]
100fb5a98:     	ldur	x8, [x29, #-0x88]
100fb5a9c:     	stur	x8, [x19, #0x18]
100fb5aa0:     	mov	w9, #0x1                ; =1
100fb5aa4:     	ldurb	w8, [x29, #-0xc8]
100fb5aa8:     	ldr	x11, [sp, #0x30]
100fb5aac:     	cbz	x9, 0x100fb5af0 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xe04>
100fb5ab0:     	b	0x100fb5a54 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd68>
100fb5ab4:     	ldr	q0, [sp, #0x50]
100fb5ab8:     	sub	x19, x29, #0xd0
100fb5abc:     	stur	q0, [x19, #0x8]
100fb5ac0:     	ldr	x8, [sp, #0x60]
100fb5ac4:     	stur	x8, [x19, #0x18]
100fb5ac8:     	ldurb	w8, [x29, #-0xc8]
100fb5acc:     	ldr	x11, [sp, #0x30]
100fb5ad0:     	b	0x100fb5a54 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd68>
100fb5ad4:     	ldurb	w8, [x29, #-0xc8]
100fb5ad8:     	sub	x19, x29, #0xd0
100fb5adc:     	cmp	w8, #0x17
100fb5ae0:     	ldr	x11, [sp, #0x30]
100fb5ae4:     	b.lo	0x100fb5a54 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd68>
100fb5ae8:     	ldur	x9, [x29, #-0xd0]
100fb5aec:     	cbnz	x9, 0x100fb5a54 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xd68>
100fb5af0:     	sub	w9, w8, #0x17
100fb5af4:     	mov	w10, #0x3               ; =3
100fb5af8:     	cmp	w8, #0x16
100fb5afc:     	csel	w8, w9, w10, hi
100fb5b00:     	and	w8, w8, #0xff
100fb5b04:     	cmp	w8, #0x1
100fb5b08:     	b.gt	0x100fb5b24 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xe38>
100fb5b0c:     	cbnz	w8, 0x100fb5bf8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf0c>
100fb5b10:     	mov	x19, #0x0               ; =0
100fb5b14:     	ldur	x27, [x29, #-0xc0]
100fb5b18:     	ldr	x8, [sp, #0x38]
100fb5b1c:     	cbnz	x8, 0x100fb5bbc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xed0>
100fb5b20:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5b24:     	cmp	w8, #0x2
100fb5b28:     	b.ne	0x100fb5b78 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xe8c>
100fb5b2c:     	mov	x27, #0x0               ; =0
100fb5b30:     	adrp	x8, 0x102d13000 <_anon.49087316163ed4345739c0711faf6653.2247+0x2889>
100fb5b34:     	add	x8, x8, #0xfc0
100fb5b38:     	ldp	q0, q1, [x8]
100fb5b3c:     	stp	q0, q1, [x20, #0x50]
100fb5b40:     	ldp	q0, q1, [x8, #0x40]
100fb5b44:     	stp	q0, q1, [x20, #0x90]
100fb5b48:     	ldp	q0, q1, [x8, #0x60]
100fb5b4c:     	stp	q0, q1, [x20, #0xb0]
100fb5b50:     	ldp	q0, q1, [x8, #0x20]
100fb5b54:     	ldur	x8, [x29, #-0xc0]
100fb5b58:     	str	x8, [x20, #0x50]
100fb5b5c:     	mov	w8, #0x80               ; =128
100fb5b60:     	strb	w8, [x20, #0x63]
100fb5b64:     	mov	w19, #0x3               ; =3
100fb5b68:     	stp	q0, q1, [x20, #0x70]
100fb5b6c:     	ldr	x8, [sp, #0x38]
100fb5b70:     	cbnz	x8, 0x100fb5bbc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xed0>
100fb5b74:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5b78:     	str	xzr, [x21, #0x108]
100fb5b7c:     	ldr	w1, [x11, #0x64]
100fb5b80:     	str	wzr, [x11, #0x64]
100fb5b84:     	cbz	w1, 0x100fb5b94 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xea8>
100fb5b88:     	mov	w8, #0x7548             ; =30024
100fb5b8c:     	add	x0, x21, x8
100fb5b90:     	bl	0x100db7c24 <__RNvMs0_NtCsgTc3P8JF8aG_8otter_vm10cold_frameNtB5_13ColdFramePool7release>
100fb5b94:     	ldr	x8, [x20, #0x10]
100fb5b98:     	cbz	x8, 0x100fb5bac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xec0>
100fb5b9c:     	ldur	q0, [x19, #0x8]
100fb5ba0:     	str	q0, [x8]
100fb5ba4:     	ldur	x9, [x19, #0x18]
100fb5ba8:     	str	x9, [x8, #0x10]
100fb5bac:     	mov	w27, #0xa               ; =10
100fb5bb0:     	mov	w19, #0x6               ; =6
100fb5bb4:     	ldr	x8, [sp, #0x38]
100fb5bb8:     	cbz	x8, 0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5bbc:     	mov	x20, #-0x1              ; =-1
100fb5bc0:     	ldaddl	x20, x8, [x8]
100fb5bc4:     	cmp	x8, #0x1
100fb5bc8:     	b.ne	0x100fb5bd8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xeec>
100fb5bcc:     	dmb	ishld
100fb5bd0:     	add	x0, sp, #0x38
100fb5bd4:     	bl	0x100e5eb48 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space12ChunkPayloadE9drop_slowBK_>
100fb5bd8:     	ldr	x8, [sp, #0x40]
100fb5bdc:     	ldaddl	x20, x8, [x8]
100fb5be0:     	cmp	x8, #0x1
100fb5be4:     	b.ne	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5be8:     	dmb	ishld
100fb5bec:     	ldr	x0, [sp, #0x18]
100fb5bf0:     	bl	0x100e5ece0 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space9CodeSpaceE9drop_slowBK_>
100fb5bf4:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5bf8:     	ldrb	w8, [x22, #0x112]
100fb5bfc:     	cmp	w8, #0xff
100fb5c00:     	b.eq	0x100fb5c1c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf30>
100fb5c04:     	ldr	x9, [x22, #0x1c8]
100fb5c08:     	ldr	x8, [x22, #0x190]
100fb5c0c:     	cmp	x9, #0x8
100fb5c10:     	csel	x8, x8, x9, hi
100fb5c14:     	lsr	x10, x8, #32
100fb5c18:     	cbz	x10, 0x100fb5c6c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xf80>
100fb5c1c:     	add	x0, sp, #0xc0
100fb5c20:     	mov	x1, x22
100fb5c24:     	bl	0x100df7da8 <__RNvMs2_NtCsgTc3P8JF8aG_8otter_vm16activation_stackNtB5_15ActivationStack14pending_packet>
100fb5c28:     	ldrb	w8, [sp, #0xd2]
100fb5c2c:     	cmp	w8, #0xff
100fb5c30:     	b.eq	0x100fb5bac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xec0>
100fb5c34:     	mov	x27, #0x0               ; =0
100fb5c38:     	add	x8, sp, #0xc0
100fb5c3c:     	ldp	q0, q1, [x8, #0x40]
100fb5c40:     	stp	q0, q1, [x20, #0x90]
100fb5c44:     	ldp	q0, q1, [x8, #0x60]
100fb5c48:     	stp	q0, q1, [x20, #0xb0]
100fb5c4c:     	ldp	q0, q1, [x8]
100fb5c50:     	stp	q0, q1, [x20, #0x50]
100fb5c54:     	ldp	q0, q1, [x8, #0x20]
100fb5c58:     	stp	q0, q1, [x20, #0x70]
100fb5c5c:     	mov	w19, #0x3               ; =3
100fb5c60:     	ldr	x8, [sp, #0x38]
100fb5c64:     	cbnz	x8, 0x100fb5bbc <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xed0>
100fb5c68:     	b	0x100fb4de8 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xfc>
100fb5c6c:     	mov	x27, #0x0               ; =0
100fb5c70:     	ldr	x10, [x22, #0x188]
100fb5c74:     	ldp	q0, q1, [x22, #0x100]
100fb5c78:     	ldp	q2, q3, [x22, #0x120]
100fb5c7c:     	stp	q2, q3, [x20, #0x70]
100fb5c80:     	stp	q0, q1, [x20, #0x50]
100fb5c84:     	ldp	q0, q1, [x22, #0x140]
100fb5c88:     	ldp	q2, q3, [x22, #0x160]
100fb5c8c:     	stp	q2, q3, [x20, #0xb0]
100fb5c90:     	stp	q0, q1, [x20, #0x90]
100fb5c94:     	mov	w11, #0xff              ; =255
100fb5c98:     	strb	w11, [x22, #0x112]
100fb5c9c:     	add	x11, x22, #0x188
100fb5ca0:     	cmp	x9, #0x8
100fb5ca4:     	csel	x9, x10, x11, hi
100fb5ca8:     	str	x9, [x20, #0x68]
100fb5cac:     	str	w8, [x20, #0x70]
100fb5cb0:     	mov	w19, #0x3               ; =3
100fb5cb4:     	b	0x100fb5264 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x578>
100fb5cb8:     	ldr	x9, [sp, #0x30]
100fb5cbc:     	ldr	w8, [x9, #0x4]
100fb5cc0:     	cmn	w8, #0x1
100fb5cc4:     	b.eq	0x100fb525c <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x570>
100fb5cc8:     	add	w8, w8, #0x1
100fb5ccc:     	str	w8, [x9, #0x4]
100fb5cd0:     	mov	w26, #0xff              ; =255
100fb5cd4:     	ldr	x8, [sp, #0x20]
100fb5cd8:     	tbz	w8, #0x0, 0x100fb53ac <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6c0>
100fb5cdc:     	b	0x100fb5398 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x6ac>
100fb5ce0:     	ldur	q0, [x8, #0x1]
100fb5ce4:     	str	q0, [sp, #0x80]
100fb5ce8:     	ldr	x8, [x8, #0x10]
100fb5cec:     	stur	x8, [sp, #0x8f]
100fb5cf0:     	ldr	q0, [sp, #0x80]
100fb5cf4:     	str	q0, [sp, #0xa0]
100fb5cf8:     	ldur	x8, [sp, #0x8f]
100fb5cfc:     	b	0x100fb5304 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x618>
100fb5d00:     	mov	w8, #0x1                ; =1
100fb5d04:     	stp	x8, x23, [x21, #0x108]
100fb5d08:     	ldur	q0, [x29, #-0x98]
100fb5d0c:     	sub	x19, x29, #0xd0
100fb5d10:     	stur	q0, [x19, #0x8]
100fb5d14:     	ldur	x8, [x29, #-0x88]
100fb5d18:     	b	0x100fb5ac4 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0xdd8>
100fb5d1c:     	bl	0x101b696c8 <__RNvMs2_NtNtCsgTc3P8JF8aG_8otter_vm10native_abi5frameNtB5_5Frame20derive_logical_depth>
100fb5d20:     	mov	x26, x0
100fb5d24:     	b	0x100fb4f00 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x214>
100fb5d28:     	mov	x19, x0
100fb5d2c:     	ldr	x8, [sp, #0x40]
100fb5d30:     	mov	x9, #-0x1               ; =-1
100fb5d34:     	ldaddl	x9, x8, [x8]
100fb5d38:     	cmp	x8, #0x1
100fb5d3c:     	b.ne	0x100fb5d90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x10a4>
100fb5d40:     	dmb	ishld
100fb5d44:     	ldr	x0, [sp, #0x18]
100fb5d48:     	bl	0x100e5ece0 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space9CodeSpaceE9drop_slowBK_>
100fb5d4c:     	b	0x100fb5d90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x10a4>
100fb5d50:     	bl	0x101afab30 <__RNvNtCsjIpoRzUjyHQ_4core9panicking16panic_in_cleanup>
100fb5d54:     	mov	x19, x0
100fb5d58:     	ldr	x8, [sp, #0x40]
100fb5d5c:     	mov	x9, #-0x1               ; =-1
100fb5d60:     	ldaddl	x9, x8, [x8]
100fb5d64:     	cmp	x8, #0x1
100fb5d68:     	b.ne	0x100fb5d90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x10a4>
100fb5d6c:     	dmb	ishld
100fb5d70:     	ldr	x0, [sp, #0x18]
100fb5d74:     	bl	0x100e5ece0 <__RNvMsn_NtCs6zY7rJBZy9D_5alloc4syncINtB5_3ArcNtNtCsgTc3P8JF8aG_8otter_vm10code_space9CodeSpaceE9drop_slowBK_>
100fb5d78:     	b	0x100fb5d90 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x10a4>
100fb5d7c:     	bl	0x101afab30 <__RNvNtCsjIpoRzUjyHQ_4core9panicking16panic_in_cleanup>
100fb5d80:     	b	0x100fb5d84 <__RNvNtNtCsgTc3P8JF8aG_8otter_vm6interp13call_dispatch16interpreter_turn+0x1098>
100fb5d84:     	mov	x19, x0
100fb5d88:     	add	x0, sp, #0x38
100fb5d8c:     	bl	0x100c47490 <__RINvNtCsjIpoRzUjyHQ_4core3ptr9drop_glueINtNtB4_6option6OptionNtNtCsgTc3P8JF8aG_8otter_vm17execution_context16ExecutionContextEEB11_>
100fb5d90:     	mov	x0, x19
100fb5d94:     	bl	0x101bb66e0 <dyld_stub_binder+0x101bb66e0>
100fb5d98:     	bl	0x101afab30 <__RNvNtCsjIpoRzUjyHQ_4core9panicking16panic_in_cleanup>
